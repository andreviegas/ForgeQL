//! Makefile language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for GNU Make files using
//! `tree-sitter-make`. Claims `.mk` plus the well-known `Makefile` /
//! `makefile` / `GNUmakefile` names (extensionless names match through the
//! registry's file-name fallback).
//!
//! Rules index as `function` rows named by their target list; `VAR = value`
//! and `VAR != cmd` assignments as `variable`; `define` blocks as `macro`;
//! `include` lines as `import`; `ifeq`/`ifdef` blocks as `if` — so a rename
//! sweep over a variable like `FOO` finds its assignment and every rule that
//! carries it by `node_id`.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// Makefile language support for ForgeQL.
pub struct MakeLanguage;

/// Static configuration for Make.
static MAKE_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Returns the static Make language configuration, loaded from
/// `config/make.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `make.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn make_config() -> &'static LanguageConfig {
    MAKE_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/make.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded make.json must be valid");
        json_config.into_language_config()
    })
}

/// The trimmed text of a node's field, if present and non-empty.
fn field_text(node: tree_sitter::Node<'_>, field: &str, source: &[u8]) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    let text = node_text(source, child).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The trimmed text of a node's first child of `kind`, if any.
fn child_text(node: tree_sitter::Node<'_>, kind: &str, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let child = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == kind)?;
    let text = node_text(source, child).trim().to_string();
    (!text.is_empty()).then_some(text)
}

impl LanguageSupport for MakeLanguage {
    fn name(&self) -> &'static str {
        "make"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["mk", "makefile", "gnumakefile"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_make::LANGUAGE.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        match node.kind() {
            // `all: deps` — a rule is named by its target list text.
            "rule" => child_text(node, "targets", source),
            // `VAR = value` / `VAR != cmd` / `define VAR` — the `name` field.
            "variable_assignment" | "shell_assignment" | "define_directive" => {
                field_text(node, "name", source)
            }
            // `include other.mk` — named by the included file list.
            "include_directive" => field_text(node, "filenames", source),
            _ => None,
        }
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        make_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        make_config()
    }
}

/// Convenience registry containing only Make support.
#[must_use]
pub fn make_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(MakeLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real Make grammar and return every
    /// `(node_kind, extract_name)` where a name is produced.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = MakeLanguage;
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

    const SAMPLE: &str = "CC := gcc\nCFLAGS = -Wall -O2\n\ninclude common.mk\n\nall: main.o util.o\n\tcc -o app main.o util.o\n\nclean:\n\trm -f *.o app\n";

    #[test]
    fn embedded_config_is_valid() {
        let cfg = make_config();
        assert!(cfg.kind_map_lookup("rule").is_some());
        assert!(cfg.kind_map_lookup("variable_assignment").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = MakeLanguage;
        assert_eq!(lang.map_kind("rule"), Some("function"));
        assert_eq!(lang.map_kind("variable_assignment"), Some("variable"));
        assert_eq!(lang.map_kind("define_directive"), Some("macro"));
        assert_eq!(lang.map_kind("include_directive"), Some("import"));
        assert_eq!(lang.map_kind("recipe_line"), None);
    }

    #[test]
    fn registry_resolves_makefile_names() {
        let registry = make_registry();
        for file in ["rules.mk", "Makefile", "makefile", "GNUmakefile"] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "make");
        }
    }

    #[test]
    fn rules_named_by_targets() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("rule".to_string(), "all".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("rule".to_string(), "clean".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn assignments_named_by_variable() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("variable_assignment".to_string(), "CC".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("variable_assignment".to_string(), "CFLAGS".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn includes_named_by_file_list() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("include_directive".to_string(), "common.mk".to_string())),
            "names: {got:?}"
        );
    }
}
