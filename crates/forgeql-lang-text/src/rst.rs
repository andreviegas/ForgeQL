//! reStructuredText language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for `.rst`/`.rest` documents using
//! `tree-sitter-rst`, mirroring the Markdown module's shape: sections index
//! as `section` rows named by their title (nested sections nest), titles as
//! `heading`, paragraphs and list items by their normalized text snippet,
//! `.. directive::` blocks as `macro_call` named by the directive type,
//! `:field:` entries as `pair`, and `.. |sub| replace::` definitions as
//! `variable` — so documentation that mentions a symbol is reachable by the
//! same `FIND` sweep that finds the symbol in code.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// reStructuredText language support for ForgeQL.
pub struct RstLanguage;

/// Static configuration for reStructuredText.
static RST_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static reStructuredText language configuration, loaded from
/// `config/rst.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `rst.json` is malformed (should never happen — the
/// file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn rst_config() -> &'static LanguageConfig {
    RST_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/rst.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded rst.json must be valid");
        json_config.into_language_config()
    })
}

/// Collapse runs of whitespace to single spaces; `None` when empty.
fn normalize_text(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

/// The first child of `node` with the given kind.
fn child_of_kind<'t>(node: tree_sitter::Node<'t>, kind: &str) -> Option<tree_sitter::Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| child.kind() == kind)
}

impl LanguageSupport for RstLanguage {
    fn name(&self) -> &'static str {
        "rst"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["rst", "rest"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_rst::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            // A section is named by its title; the title itself doubles as a
            // heading row (mirrors the Markdown section/heading pair).
            "section" => {
                let title = child_of_kind(node, "title")?;
                normalize_text(&node_text(source, title))
            }
            // Prose blocks and headings are named by their normalized text.
            "title" | "paragraph" | "list_item" => normalize_text(&node_text(source, node)),
            // `.. code-block:: rust` — the directive type; `.. |version|
            // replace:: 1.0` — the substitution. Both are the `name` field.
            "directive" | "substitution_definition" => {
                let name = node.child_by_field_name("name")?;
                normalize_text(&node_text(source, name))
            }
            // `:returns: …` — named by the field name.
            "field" => {
                let name = child_of_kind(node, "field_name")?;
                normalize_text(&node_text(source, name))
            }
            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        rst_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        rst_config()
    }
}

/// Convenience registry containing only reStructuredText support.
#[must_use]
pub fn rst_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(RstLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real reStructuredText grammar and return every
    /// `(node_kind, extract_name)` where a name is produced.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = RstLanguage;
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

    const SAMPLE: &str = "Configuration\n=============\n\nThe FOO macro controls the mode.\n\nUsage\n-----\n\n.. code-block:: c\n\n   #define FOO 1\n\n:returns: nothing\n";

    #[test]
    fn embedded_config_is_valid() {
        let cfg = rst_config();
        assert!(cfg.kind_map_lookup("section").is_some());
        assert!(cfg.kind_map_lookup("directive").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = RstLanguage;
        assert_eq!(lang.map_kind("section"), Some("section"));
        assert_eq!(lang.map_kind("title"), Some("heading"));
        assert_eq!(lang.map_kind("paragraph"), Some("paragraph"));
        assert_eq!(lang.map_kind("directive"), Some("macro_call"));
        assert_eq!(lang.map_kind("literal_block"), Some("code_block"));
        assert_eq!(lang.map_kind("emphasis"), None);
    }

    #[test]
    fn registry_resolves_rst_extensions() {
        let registry = rst_registry();
        for file in ["index.rst", "api.rest"] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "rst");
        }
    }

    #[test]
    fn sections_named_by_title() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("section".to_string(), "Configuration".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("section".to_string(), "Usage".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn paragraphs_named_by_normalized_text() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&(
                "paragraph".to_string(),
                "The FOO macro controls the mode.".to_string()
            )),
            "names: {got:?}"
        );
    }

    #[test]
    fn directives_named_by_type() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("directive".to_string(), "code-block".to_string())),
            "names: {got:?}"
        );
    }
}
