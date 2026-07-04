//! Markdown language support for ForgeQL.
//!
//! This crate implements [`LanguageSupport`] for Markdown using
//! `tree-sitter-md`.  Register an instance with [`LanguageRegistry`]
//! at application startup to enable indexing and analysis of Markdown
//! documents.
//!
//! ```ignore
//! use forgeql_lang_markdown::MarkdownLanguage;
//! use forgeql_core::ast::lang::LanguageRegistry;
//!
//! let registry = LanguageRegistry::new(vec![Arc::new(MarkdownLanguage)]);
//! ```

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// Markdown language support for ForgeQL.
pub struct MarkdownLanguage;

/// Static configuration for Markdown.
static MARKDOWN_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static Markdown language configuration, loaded from
/// `config/md.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `md.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn markdown_config() -> &'static LanguageConfig {
    MARKDOWN_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/md.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded md.json must be valid");
        json_config.into_language_config()
    })
}

fn normalize_markdown_text(text: &str) -> Option<String> {
    let normalized = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

impl LanguageSupport for MarkdownLanguage {
    fn name(&self) -> &'static str {
        "markdown"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["md", "mdx"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_md::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            // ATX headings (`# Heading`): strip the `#` level markers and trim.
            // CommonMark allows an optional closing `##` sequence — strip that too.
            "atx_heading" => {
                let text = node_text(source, node);
                let stripped = text.trim_start_matches('#').trim();
                let stripped = stripped.trim_end_matches('#').trim();
                if stripped.is_empty() {
                    None
                } else {
                    Some(stripped.to_string())
                }
            }

            // Setext headings: the text comes before the underline child.
            // Setext headings: the text comes before the underline child.
            "setext_heading" => {
                for i in 0..node.named_child_count() {
                    if let Some(child) = node.named_child(i)
                        && child.kind() != "setext_h1_underline"
                        && child.kind() != "setext_h2_underline"
                    {
                        let text = node_text(source, child);
                        let trimmed = text.trim().to_string();
                        if !trimmed.is_empty() {
                            return Some(trimmed);
                        }
                    }
                }
                None
            }

            // Section: tree-sitter-md produces a `section` node whose first
            // named child is always the heading — delegate to it.
            "section" => node
                .named_child(0)
                .and_then(|heading| self.extract_name(heading, source)),

            // Fenced code blocks: extract the info string (e.g. `rust` from ```rust).
            "fenced_code_block" => {
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i)
                        && child.kind() == "info_string"
                    {
                        let text = node_text(source, child).trim().to_string();
                        if !text.is_empty() {
                            return Some(text);
                        }
                    }
                }
                None
            }

            // Prose/content blocks: index full normalized text so labels and
            // identifiers mentioned in docs become searchable with FIND.
            "paragraph" | "list_item" | "block_quote" | "pipe_table" | "link_definition" => {
                normalize_markdown_text(&node_text(source, node))
            }

            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        markdown_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        markdown_config()
    }
}

// -----------------------------------------------------------------------
// Convenience: build a default Markdown registry
// -----------------------------------------------------------------------

/// Build a [`LanguageRegistry`] containing only Markdown support.
#[must_use]
pub fn markdown_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(MarkdownLanguage)])
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code")]
mod tests {
    use super::*;

    #[test]
    fn map_kind_covers_heading_kinds() {
        let lang = MarkdownLanguage;
        assert_eq!(lang.map_kind("atx_heading"), Some("heading"));
        assert_eq!(lang.map_kind("setext_heading"), Some("heading"));
        assert_eq!(lang.map_kind("section"), Some("section"));
    }

    #[test]
    fn map_kind_covers_block_kinds() {
        let lang = MarkdownLanguage;
        assert_eq!(lang.map_kind("fenced_code_block"), Some("code_block"));
        assert_eq!(lang.map_kind("indented_code_block"), Some("code_block"));
        assert_eq!(lang.map_kind("list_item"), Some("list_item"));
        assert_eq!(lang.map_kind("paragraph"), Some("paragraph"));
        assert_eq!(lang.map_kind("pipe_table"), Some("table"));
    }

    #[test]
    fn map_kind_returns_none_for_unknown() {
        let lang = MarkdownLanguage;
        assert_eq!(lang.map_kind("unknown_node_type_xyz"), None);
    }

    #[test]
    fn registry_resolves_md_extension() {
        let registry = markdown_registry();
        let path = std::path::Path::new("README.md");
        let lang = registry.language_for_path(path);
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name(), "markdown");
    }

    #[test]
    fn registry_resolves_mdx_extension() {
        let registry = markdown_registry();
        let path = std::path::Path::new("component.mdx");
        let lang = registry.language_for_path(path);
        assert!(lang.is_some());
        assert_eq!(lang.unwrap().name(), "markdown");
    }

    #[test]
    fn registry_returns_none_for_unknown_extension() {
        let registry = markdown_registry();
        let path = std::path::Path::new("script.sh");
        assert!(registry.language_for_path(path).is_none());
    }

    #[test]
    fn extract_name_atx_heading() {
        fn find_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
            if node.kind() == kind {
                return Some(node);
            }
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i)
                    && let Some(found) = find_kind(child, kind)
                {
                    return Some(found);
                }
            }
            None
        }

        let lang = MarkdownLanguage;
        let source = b"## Phase 0: Markdown Support";
        let ts_lang = lang.tree_sitter_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        if let Some(heading) = find_kind(root, "atx_heading") {
            let name = lang.extract_name(heading, source);
            assert_eq!(name.as_deref(), Some("Phase 0: Markdown Support"));
        }
        // If tree-sitter-md wraps the heading in a section node, extract_name
        // on the section should still return the heading text.
        if let Some(section) = find_kind(root, "section") {
            let name = lang.extract_name(section, source);
            assert_eq!(name.as_deref(), Some("Phase 0: Markdown Support"));
        }
    }

    #[test]
    #[expect(clippy::expect_used, reason = "test code")]
    fn extract_name_paragraph_normalizes_whitespace() {
        fn find_kind<'a>(node: tree_sitter::Node<'a>, kind: &str) -> Option<tree_sitter::Node<'a>> {
            if node.kind() == kind {
                return Some(node);
            }
            for i in 0..node.named_child_count() {
                if let Some(child) = node.named_child(i)
                    && let Some(found) = find_kind(child, kind)
                {
                    return Some(found);
                }
            }
            None
        }

        let lang = MarkdownLanguage;
        let source = b"This   is  k_sys_work_q_init\nwith   extra spacing.";
        let ts_lang = lang.tree_sitter_language();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&ts_lang).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let para = find_kind(root, "paragraph").expect("paragraph should be present");
        let name = lang.extract_name(para, source);
        assert_eq!(
            name.as_deref(),
            Some("This is k_sys_work_q_init with extra spacing.")
        );
    }
}
