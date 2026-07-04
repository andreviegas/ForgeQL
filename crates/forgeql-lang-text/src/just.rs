//! justfile language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for `just` command-runner files using
//! `tree-sitter-just`. Claims the `just`/`justfile` keys, so `x.just`,
//! `justfile`, `.justfile`, and `Justfile` all resolve (extensionless names
//! match through the registry's file-name fallback).
//!
//! Recipes index as `function` rows named by the recipe name; `:=`
//! assignments and `alias` lines as `variable`; `set` lines as `pair`;
//! `mod` lines as `namespace`; `import` lines as `import` — so an agent can
//! address one recipe of a large justfile and edit it by `node_id`.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// justfile language support for ForgeQL.
pub struct JustLanguage;

/// Static configuration for just.
static JUST_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static just language configuration, loaded from
/// `config/just.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `just.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn just_config() -> &'static LanguageConfig {
    JUST_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/just.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded just.json must be valid");
        json_config.into_language_config()
    })
}

/// The text of a node's field, if present and non-empty.
fn field_text(node: tree_sitter::Node<'_>, field: &str, source: &[u8]) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    let text = node_text(source, child).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl LanguageSupport for JustLanguage {
    fn name(&self) -> &'static str {
        "just"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["just", "justfile"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_just::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            // A recipe is named by its header's `name` field.
            "recipe" => {
                let mut cursor = node.walk();
                let header = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "recipe_header")?;
                field_text(header, "name", source)
            }
            // `x := …`, `alias x := …`, `set x := …` — named by the left side.
            "assignment" | "alias" => field_text(node, "left", source),
            // Settings are named by the left side too, but `set shell := […]`
            // is a dedicated grammar branch where `shell` is a keyword, not a
            // `left` field — fall back to the token following `set`.
            "setting" => field_text(node, "left", source).or_else(|| {
                let text = node_text(source, node);
                let name = text.split_whitespace().nth(1)?.to_string();
                (!name.is_empty()).then_some(name)
            }),
            // `mod name …` — named by the `name` field.
            "module" => field_text(node, "name", source),
            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        just_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        just_config()
    }
}

/// Convenience registry containing only just support.
#[must_use]
pub fn just_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(JustLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real just grammar and return every
    /// `(node_kind, extract_name)` where a name is produced.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = JustLanguage;
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

    const SAMPLE: &str = "set shell := [\"bash\", \"-c\"]\n\nversion := \"1.0\"\n\nalias b := build\n\nbuild:\n    cargo build\n\ntest filter='':\n    cargo test {{filter}}\n";

    #[test]
    fn embedded_config_is_valid() {
        let cfg = just_config();
        assert!(cfg.kind_map_lookup("recipe").is_some());
        assert!(cfg.kind_map_lookup("assignment").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = JustLanguage;
        assert_eq!(lang.map_kind("recipe"), Some("function"));
        assert_eq!(lang.map_kind("assignment"), Some("variable"));
        assert_eq!(lang.map_kind("setting"), Some("pair"));
        assert_eq!(lang.map_kind("comment"), Some("comment"));
        assert_eq!(lang.map_kind("recipe_line"), None);
    }

    #[test]
    fn registry_resolves_justfile_names() {
        let registry = just_registry();
        for file in ["build.just", "justfile", ".justfile", "Justfile"] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "just");
        }
    }

    #[test]
    fn recipes_named_by_header_name() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("recipe".to_string(), "build".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("recipe".to_string(), "test".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn assignments_aliases_and_settings_named_by_left_side() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("assignment".to_string(), "version".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("alias".to_string(), "b".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("setting".to_string(), "shell".to_string())),
            "names: {got:?}"
        );
    }
}
