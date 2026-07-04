//! TOML language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for TOML using `tree-sitter-toml`. Registered
//! for the `toml` and `lock` extensions — `Cargo.lock` is itself TOML, so the
//! same grammar makes both Cargo manifests node-addressable: a `version = "…"`
//! pair, or a `[[package]]` entry, can be located and edited by node handle
//! instead of by raw text.
//!
//! As with JSON/YAML, the useful addressable units are key-value members. Each
//! `pair` is indexed under its key; each `[table]` / `[[table-array]]` is named
//! after its `name`/`id`/`key` member when present, otherwise after its header
//! key — so every section of a manifest gets a stable `node_id`.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// TOML language support for ForgeQL.
pub struct TomlLanguage;

/// Static configuration for TOML.
static TOML_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Pair keys, in priority order, used to name an enclosing table by a member
/// value so the table itself is addressable (e.g. a `[[package]]` named "serde").
const IDENTIFIER_KEYS: &[&str] = &["name", "id", "key"];

/// Key node kinds (bare, quoted, dotted).
const KEY_KINDS: &[&str] = &["bare_key", "quoted_key", "dotted_key"];

/// Table node kinds (`[table]` and `[[table-array]]`).
const TABLE_KINDS: &[&str] = &["table", "table_array_element"];

/// Returns the static TOML language configuration, loaded from
/// `config/toml.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `toml.json` is malformed (should never happen — the
/// file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn toml_config() -> &'static LanguageConfig {
    TOML_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/toml.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded toml.json must be valid");
        json_config.into_language_config()
    })
}

/// Strip surrounding single or double quotes from a TOML key or string scalar.
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

/// The (unquoted) text of the first key-kind child of `node`, if any.
fn key_text(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if KEY_KINDS.contains(&child.kind()) {
            let name = unquote(&node_text(source, child)).to_string();
            return (!name.is_empty()).then_some(name);
        }
    }
    None
}

/// Name a table by the value of its first identifier-like member pair, if any.
fn table_identifier(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "pair" {
            continue;
        }
        let Some(key) = key_text(child, source) else {
            continue;
        };
        if !IDENTIFIER_KEYS.contains(&key.as_str()) {
            continue;
        }
        // The value is the last named child of the pair (`key = value`).
        let value = child.named_child(child.named_child_count().saturating_sub(1));
        if let Some(value) = value {
            let name = unquote(&node_text(source, value)).to_string();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

impl LanguageSupport for TomlLanguage {
    fn name(&self) -> &'static str {
        "toml"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["toml", "lock"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_toml::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        let kind = node.kind();
        if kind == "pair" {
            // Key-value members: index under the (unquoted) key text.
            key_text(node, source)
        } else if TABLE_KINDS.contains(&kind) {
            // Tables: name after an identifier-like member (e.g. a `[[package]]`
            // by its `name`), else after the table header key.
            table_identifier(node, source).or_else(|| key_text(node, source))
        } else {
            None
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        toml_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        toml_config()
    }
}

/// Convenience registry containing only TOML support.
#[must_use]
pub fn toml_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(TomlLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real TOML grammar and return every
    /// `(node_kind, extract_name)` where a name is produced — the end-to-end
    /// check that our node-kind assumptions match the grammar.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = TomlLanguage;
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

    #[test]
    fn embedded_config_is_valid() {
        let cfg = toml_config();
        assert!(cfg.kind_map_lookup("pair").is_some());
        assert!(cfg.kind_map_lookup("table").is_some());
    }

    #[test]
    fn pairs_are_named_by_key() {
        // The Cargo.toml version-bump case: the `version` pair is addressable.
        let got = names("version = \"0.80.9\"\nedition = \"2024\"\n");
        let only: Vec<&str> = got.iter().map(|(_, n)| n.as_str()).collect();
        assert!(only.contains(&"version"), "names: {got:?}");
        assert!(only.contains(&"edition"), "names: {got:?}");
    }

    #[test]
    fn tables_named_by_header_or_member() {
        // `[workspace.package]` → header key; `[[package]]` → its `name` member.
        // Mirrors Cargo.toml + Cargo.lock structure.
        let src = "[workspace.package]\nversion = \"0.80.9\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0\"\n";
        let got = names(src);
        let only: Vec<&str> = got.iter().map(|(_, n)| n.as_str()).collect();
        assert!(only.contains(&"workspace.package"), "names: {got:?}");
        assert!(only.contains(&"serde"), "names: {got:?}");
        assert!(only.contains(&"version"), "names: {got:?}");
    }
}
