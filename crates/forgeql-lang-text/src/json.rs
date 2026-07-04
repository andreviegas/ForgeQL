//! JSON language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for JSON using
//! `tree-sitter-json`.  Register an instance with [`LanguageRegistry`]
//! at application startup to enable indexing and analysis of JSON
//! documents.
//!
//! Unlike code languages, JSON has no functions or types — its useful
//! addressable units are object members.  Each `pair` is indexed under
//! its key, and each `object` is named after the value of its `name`,
//! `id`, `key`, `title`, or `alias` member when one is present.  This
//! makes every entry of a large data file (e.g. a golden-test corpus)
//! individually addressable by a stable `node_id`.
//!
//! ```ignore
//! use forgeql_lang_json::JsonLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(JsonLanguage)]);
//! ```

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// JSON language support for ForgeQL.
pub struct JsonLanguage;

/// Static configuration for JSON.
static JSON_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Member keys, in priority order, used to name an enclosing object so the
/// object itself becomes addressable (e.g. a golden-test case named by its
/// `name` field).
const IDENTIFIER_KEYS: &[&str] = &["name", "id", "key", "title", "alias"];

/// Returns the static JSON language configuration, loaded from
/// `config/json.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `json.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn json_config() -> &'static LanguageConfig {
    JSON_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/json.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded json.json must be valid");
        json_config.into_language_config()
    })
}

/// Strip the surrounding double quotes from a JSON string literal's text.
///
/// Falls back to the trimmed input when the text is not a quoted string
/// (e.g. a JSON5 unquoted key or a numeric key).
fn unquote(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(trimmed)
}

/// Name an object by the value of its first identifier-like member, if any.
fn object_identifier(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "pair" {
            continue;
        }
        let Some(key) = child.child_by_field_name("key") else {
            continue;
        };
        let key_text = node_text(source, key);
        let key_name = unquote(&key_text);
        if !IDENTIFIER_KEYS.contains(&key_name) {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        let value_text = node_text(source, value);
        let value_name = unquote(&value_text).to_string();
        if !value_name.is_empty() {
            return Some(value_name);
        }
    }
    None
}

impl LanguageSupport for JsonLanguage {
    fn name(&self) -> &'static str {
        "json"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["json", "jsonc"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_json::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            // Object members: index under the (unquoted) key text.
            "pair" => {
                let key = node.child_by_field_name("key")?;
                let name = unquote(&node_text(source, key)).to_string();
                (!name.is_empty()).then_some(name)
            }

            // Objects: name after an identifier-like member so whole entries
            // (e.g. golden-test cases) are addressable by a stable node_id.
            "object" => object_identifier(node, source),

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        json_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        json_config()
    }
}

// -----------------------------------------------------------------------
// Convenience: build a default JSON registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only JSON support.
#[must_use]
pub fn json_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(JsonLanguage)])
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    /// Recursively collect every name `extract_name` would index for a tree.
    fn collect_names(
        lang: &JsonLanguage,
        node: tree_sitter::Node<'_>,
        source: &[u8],
    ) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(name) = lang.extract_name(node, source) {
            out.push(name);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            out.extend(collect_names(lang, child, source));
        }
        out
    }

    fn parse(source: &[u8]) -> tree_sitter::Tree {
        let lang = JsonLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = JsonLanguage;
        assert_eq!(lang.map_kind("object"), Some("object"));
        assert_eq!(lang.map_kind("array"), Some("array"));
        assert_eq!(lang.map_kind("pair"), Some("pair"));
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = JsonLanguage;
        assert_eq!(lang.map_kind("string"), None);
        assert_eq!(lang.map_kind("unknown_node_type_xyz"), None);
    }

    #[test]
    fn registry_resolves_json_extension() {
        let registry = json_registry();
        let path = std::path::Path::new("golden.json");
        let lang = registry.language_for_path(path);
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name(), "json");
    }

    #[test]
    fn indexes_pair_keys() {
        let source = br#"{"name": "G2", "fql": "FIND symbols", "expect_row_count": 5}"#;
        let tree = parse(source);
        let names = collect_names(&JsonLanguage, tree.root_node(), source);
        assert!(names.contains(&"name".to_string()));
        assert!(names.contains(&"fql".to_string()));
        assert!(names.contains(&"expect_row_count".to_string()));
    }

    #[test]
    fn names_object_by_identifier_member() {
        // Mirrors golden.json: an array of test-case objects keyed by "name".
        let source = br#"[{"name": "G2_kernel_sched", "fql": "FIND symbols"}]"#;
        let tree = parse(source);
        let names = collect_names(&JsonLanguage, tree.root_node(), source);
        // The enclosing object is addressable under its identifier value.
        assert!(names.contains(&"G2_kernel_sched".to_string()));
    }

    #[test]
    fn nested_objects_are_named() {
        // Inner rows like {"line": 51, "name": "thread_runq"} are addressable.
        let source = br#"{"expect_rows": [{"line": 51, "name": "thread_runq"}]}"#;
        let tree = parse(source);
        let names = collect_names(&JsonLanguage, tree.root_node(), source);
        assert!(names.contains(&"expect_rows".to_string()));
        assert!(names.contains(&"thread_runq".to_string()));
    }
}
