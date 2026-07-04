//! INI language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for INI-style configuration files using
//! `tree-sitter-ini`. Claims `.ini`/`.cfg` plus the well-known file names
//! `.editorconfig` and `.gitconfig` (extensionless names match through the
//! registry's file-name fallback).
//!
//! `[section]` blocks index as `object` rows named by the section header;
//! `key = value` settings nest inside their section as `pair` rows — the
//! same object/pair shape as the JSON/YAML/TOML family.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// INI language support for ForgeQL.
pub struct IniLanguage;

/// Static configuration for INI.
static INI_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// `(container kind, name-child kind)` pairs: each indexed construct is named
/// by the text of its first child of the given kind.
const NAME_CHILD: &[(&str, &str)] = &[("section", "section_name"), ("setting", "setting_name")];

/// Returns the static INI language configuration, loaded from
/// `config/ini.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `ini.json` is malformed (should never happen — the
/// file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn ini_config() -> &'static LanguageConfig {
    INI_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/ini.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded ini.json must be valid");
        json_config.into_language_config()
    })
}

/// Strip the surrounding `[` `]` from a section header's text.
fn strip_brackets(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .map_or(trimmed, str::trim)
}

impl LanguageSupport for IniLanguage {
    fn name(&self) -> &'static str {
        "ini"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["ini", "cfg", "editorconfig", "gitconfig"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_ini::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        let name_kind = NAME_CHILD
            .iter()
            .find(|(kind, _)| *kind == node.kind())
            .map(|(_, name_kind)| *name_kind)?;
        let mut cursor = node.walk();
        let name = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == name_kind)?;
        let text = strip_brackets(&node_text(source, name)).to_string();
        (!text.is_empty()).then_some(text)
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        ini_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        ini_config()
    }
}

/// Convenience registry containing only INI support.
#[must_use]
pub fn ini_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(IniLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real INI grammar and return every
    /// `(node_kind, extract_name)` where a name is produced.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = IniLanguage;
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

    const SAMPLE: &str =
        "root = true\n\n[core]\nautocrlf = input\neditor = vim\n\n[user]\nname = Andre\n";

    #[test]
    fn embedded_config_is_valid() {
        let cfg = ini_config();
        assert!(cfg.kind_map_lookup("section").is_some());
        assert!(cfg.kind_map_lookup("setting").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = IniLanguage;
        assert_eq!(lang.map_kind("section"), Some("object"));
        assert_eq!(lang.map_kind("setting"), Some("pair"));
        assert_eq!(lang.map_kind("comment"), Some("comment"));
        assert_eq!(lang.map_kind("setting_value"), None);
    }

    #[test]
    fn registry_resolves_ini_family_names() {
        let registry = ini_registry();
        for file in ["setup.cfg", "php.ini", ".editorconfig", ".gitconfig"] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "ini");
        }
    }

    #[test]
    fn sections_named_by_header() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("section".to_string(), "core".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("section".to_string(), "user".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn settings_named_by_key() {
        let got = names(SAMPLE);
        for key in ["root", "autocrlf", "editor", "name"] {
            assert!(
                got.contains(&("setting".to_string(), key.to_string())),
                "missing '{key}' in: {got:?}"
            );
        }
    }
}
