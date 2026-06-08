//! YAML language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for YAML using
//! `tree-sitter-yaml`.  Register an instance with [`LanguageRegistry`]
//! at application startup to enable indexing and analysis of YAML
//! documents.
//!
//! As with JSON, YAML's useful addressable units are mapping members.
//! Each mapping pair is indexed under its key, and each mapping is named
//! after the value of its `name`, `id`, `key`, `title`, or `alias` member
//! when one is present — making every entry of a configuration or data
//! file individually addressable by a stable `node_id`.
//!
//! ```ignore
//! use forgeql_lang_yaml::YamlLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(YamlLanguage)]);
//! ```

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// YAML language support for ForgeQL.
pub struct YamlLanguage;

/// Static configuration for YAML.
static YAML_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Mapping keys, in priority order, used to name an enclosing mapping so the
/// mapping itself becomes addressable (e.g. an entry named by its `name` key).
const IDENTIFIER_KEYS: &[&str] = &["name", "id", "key", "title", "alias"];

/// Mapping-pair node kinds (block and flow style).
const PAIR_KINDS: &[&str] = &["block_mapping_pair", "flow_pair"];

/// Mapping node kinds (block and flow style).
const MAPPING_KINDS: &[&str] = &["block_mapping", "flow_mapping"];

/// Returns the static YAML language configuration, loaded from
/// `config/yaml.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `yaml.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn yaml_config() -> &'static LanguageConfig {
    YAML_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/yaml.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded yaml.json must be valid");
        json_config.into_language_config()
    })
}

/// Strip surrounding single or double quotes from a YAML scalar's text.
///
/// Plain (unquoted) scalars are returned trimmed and unchanged.
fn unquote(text: &str) -> &str {
    let trimmed = text.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = trimmed
            .strip_prefix(quote)
            .and_then(|s| s.strip_suffix(quote))
        {
            return inner;
        }
    }
    trimmed
}

/// The (unquoted) text of a mapping pair's key, if any.
fn pair_key(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let key = node.child_by_field_name("key")?;
    let name = unquote(&node_text(source, key)).to_string();
    (!name.is_empty()).then_some(name)
}

/// Name a mapping by the value of its first identifier-like member, if any.
fn mapping_identifier(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if !PAIR_KINDS.contains(&child.kind()) {
            continue;
        }
        let Some(key) = child.child_by_field_name("key") else {
            continue;
        };
        let key_text = node_text(source, key);
        if !IDENTIFIER_KEYS.contains(&unquote(&key_text)) {
            continue;
        }
        let Some(value) = child.child_by_field_name("value") else {
            continue;
        };
        let value_name = unquote(&node_text(source, value)).to_string();
        if !value_name.is_empty() {
            return Some(value_name);
        }
    }
    None
}

impl LanguageSupport for YamlLanguage {
    fn name(&self) -> &'static str {
        "yaml"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["yaml", "yml"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_yaml::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        let kind = node.kind();
        if PAIR_KINDS.contains(&kind) {
            // Mapping members: index under the (unquoted) key text.
            pair_key(node, source)
        } else if MAPPING_KINDS.contains(&kind) {
            // Mappings: name after an identifier-like member so whole entries
            // are addressable by a stable node_id.
            mapping_identifier(node, source)
        } else {
            None
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        yaml_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        yaml_config()
    }
}

// -----------------------------------------------------------------------
// Convenience: build a default YAML registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only YAML support.
#[must_use]
pub fn yaml_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(YamlLanguage)])
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
        lang: &YamlLanguage,
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
        let lang = YamlLanguage;
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.tree_sitter_language()).unwrap();
        parser.parse(source, None).unwrap()
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = YamlLanguage;
        assert_eq!(lang.map_kind("block_mapping_pair"), Some("pair"));
        assert_eq!(lang.map_kind("block_mapping"), Some("object"));
        assert_eq!(lang.map_kind("block_sequence"), Some("array"));
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = YamlLanguage;
        assert_eq!(lang.map_kind("plain_scalar"), None);
        assert_eq!(lang.map_kind("unknown_node_type_xyz"), None);
    }

    #[test]
    fn registry_resolves_yaml_extensions() {
        let registry = yaml_registry();
        for file in ["config.yaml", "config.yml"] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "yaml");
        }
    }

    #[test]
    fn indexes_mapping_keys() {
        let source = b"name: G2\nfql: FIND symbols\nexpect_row_count: 5\n";
        let tree = parse(source);
        let names = collect_names(&YamlLanguage, tree.root_node(), source);
        assert!(names.contains(&"name".to_string()));
        assert!(names.contains(&"fql".to_string()));
        assert!(names.contains(&"expect_row_count".to_string()));
    }

    #[test]
    fn names_mapping_by_identifier_member() {
        let source = b"- name: G2_kernel_sched\n  fql: FIND symbols\n";
        let tree = parse(source);
        let names = collect_names(&YamlLanguage, tree.root_node(), source);
        assert!(names.contains(&"G2_kernel_sched".to_string()));
    }
}
