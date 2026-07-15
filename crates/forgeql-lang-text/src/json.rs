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

use crate::structure::{self, StructureSpec};
use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport};
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

/// JSON's node-kind vocabulary for the shared naming ladder.
///
/// Everything about *how* a node is named lives in [`crate::structure`]; this
/// const supplies only the tree-sitter-json kind names.
const JSON_SPEC: StructureSpec = StructureSpec {
    pair_kinds: &["pair"],
    container_kinds: &["object"],
    sequence_kinds: &["array"],
    identifier_keys: IDENTIFIER_KEYS,
    unquote,
};

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
        structure::structured_name(node, source, &JSON_SPEC)
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        json_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        json_config()
    }

    fn validate_source(&self, source: &[u8], path: &std::path::Path) -> Option<Result<(), String>> {
        // JSONC permits comments and trailing commas, which a strict RFC-8259
        // parser rejects — so only the plain `.json` dialect is checked.
        if path.extension().and_then(|e| e.to_str()) == Some("jsonc") {
            return None;
        }
        Some(
            serde_json::from_slice::<serde_json::Value>(source)
                .map(|_| ())
                .map_err(|e| e.to_string()),
        )
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

    #[test]
    fn names_array_after_its_key() {
        // The `array` kind has always been in json.json's kind_map but could
        // never be emitted, because nothing named an array. It is now named
        // after the key of its nearest ancestor pair.
        let source = br#"{"steps": [1, 2]}"#;
        let tree = parse(source);
        let names = collect_names(&JsonLanguage, tree.root_node(), source);
        assert!(names.contains(&"steps".to_string()));
    }

    #[test]
    fn names_identifier_less_object_by_its_key_set() {
        // An object with no name/id/key/title/alias member used to emit no row
        // at all, and its children were reparented onto the enclosing pair.
        let source = br#"{"steps": [{"uses": "actions/checkout@v4"}]}"#;
        let tree = parse(source);
        let names = collect_names(&JsonLanguage, tree.root_node(), source);
        assert!(names.contains(&"uses".to_string()));
    }

    #[test]
    fn key_set_skeleton_survives_a_value_edit() {
        // Bumping a value must not change any node's identity: OrdinalRemapper
        // matches on (name, fql_kind, parent_ordinal), so a name that moved here
        // would hand the node a fresh ordinal on every edit.
        let before = br#"{"s": [{"uses": "actions/checkout@v4"}]}"#;
        let after = br#"{"s": [{"uses": "actions/checkout@v5"}]}"#;
        let (t1, t2) = (parse(before), parse(after));
        assert_eq!(
            collect_names(&JsonLanguage, t1.root_node(), before),
            collect_names(&JsonLanguage, t2.root_node(), after),
        );
    }

    #[test]
    fn no_name_encodes_a_position() {
        // A positional name (`steps[0]`) would follow the slot rather than the
        // node: swap two siblings and each matches the other's ordinal hint, so
        // the two nodes trade handles. Guard the ladder against regressing to it.
        let source = br#"{"steps": [{"uses": "a"}, {"uses": "b"}]}"#;
        let tree = parse(source);
        let names = collect_names(&JsonLanguage, tree.root_node(), source);
        assert!(
            names.iter().all(|n| !n.ends_with(']')),
            "a name encodes a position: {names:?}"
        );
    }

    #[test]
    fn json_declares_an_array_block_group() {
        // NOTE: this asserts only that the RULE is declared. It is NOT sufficient
        // on its own — `array_block` once shipped completely dead while this test
        // passed, because the run scanner walked raw siblings and JSON's `,`
        // separators broke every run at the first comma. Config is not behaviour.
        // The row-is-actually-emitted guard lives in the `structured_text` golden
        // suite, which asserts against a real corpus.
        let groups = json_config().block_groups();
        let spec = groups
            .iter()
            .find(|s| s.member_fql_kind == "array")
            .unwrap();
        assert_eq!(spec.block_fql_kind, "array_block");
        assert!(
            spec.min_run >= 2,
            "a run of one is not a run: {}",
            spec.min_run
        );
    }

    #[test]
    fn array_elements_are_adjacent_named_siblings_despite_commas() {
        // The precise shape that made array_block dead: JSON separates array
        // elements with `,` tokens. Those are ANONYMOUS siblings, so a run
        // scanner walking `next_sibling()` stops at the first comma and never
        // reaches min_run. `next_named_sibling()` skips them.
        //
        // This test pins the tree-sitter fact the block scanner depends on.
        let source = br#"[["a","b"],["c","d"],["e","f"]]"#;
        let tree = parse(source);
        let root = tree.root_node().named_child(0).unwrap(); // the outer array

        let first = root.named_child(0).unwrap();
        assert_eq!(first.kind(), "array");

        // Raw sibling walk hits the comma; named sibling walk reaches the element.
        assert_eq!(first.next_sibling().unwrap().kind(), ",");
        assert_eq!(first.next_named_sibling().unwrap().kind(), "array");

        let mut run = 1;
        let mut cursor = first;
        while let Some(sib) = cursor.next_named_sibling() {
            assert_eq!(sib.kind(), "array");
            run += 1;
            cursor = sib;
        }
        assert_eq!(run, 3, "all three elements must be walkable as one run");
    }
}
