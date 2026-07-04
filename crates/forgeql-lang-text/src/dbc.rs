//! DBC (Vector CAN database) language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for the `.dbc` bus-description format using
//! `tree-sitter-dbc`. DBC files define the messages on a CAN bus; the
//! grammar exposes their structure directly, so the addressable units map
//! naturally onto ForgeQL's generic kinds:
//!
//! | DBC construct                     | fql_kind   | named by             |
//! |-----------------------------------|------------|----------------------|
//! | `BO_` message                     | `object`   | message name         |
//! | `SG_` signal (nested in message)  | `field`    | signal name          |
//! | `VAL_TABLE_` value table          | `enum`     | table name           |
//! | `VAL_` signal value descriptions  | `enum`     | signal name          |
//! | `BA_DEF_` attribute definition    | `pair`     | attribute name       |
//! | `BA_` attribute value             | `pair`     | attribute name       |
//! | `EV_` environment variable        | `variable` | env var name         |
//! | `CM_` comment entry               | `comment`  | —                    |
//!
//! Signals are children of their message in the grammar, so every `SG_` gets
//! a nested `node_id` under its `BO_` — an agent can address one signal of
//! one message directly and edit it without touching the rest of the file.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// DBC language support for ForgeQL.
pub struct DbcLanguage;

/// Static configuration for DBC.
static DBC_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// `(container kind, name-child kind)` pairs: each indexed construct is named
/// by the text of its first child of the given kind.
const NAME_CHILD: &[(&str, &str)] = &[
    ("message", "message_name"),
    ("signal", "signal_name"),
    ("value_table", "value_table_name"),
    ("value_descriptions_for_signal", "signal_name"),
    ("attribute_definition", "attribute_name"),
    ("attribute_value_for_object", "attribute_name"),
    ("environment_variable", "env_var_name"),
];

/// Returns the static DBC language configuration, loaded from
/// `config/dbc.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `dbc.json` is malformed (should never happen — the
/// file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn dbc_config() -> &'static LanguageConfig {
    DBC_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/dbc.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded dbc.json must be valid");
        json_config.into_language_config()
    })
}

/// Strip surrounding double quotes from a DBC string (attribute names are
/// quoted `char_string`s; identifiers are bare).
fn unquote(text: &str) -> &str {
    let trimmed = text.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(trimmed)
}

impl LanguageSupport for DbcLanguage {
    fn name(&self) -> &'static str {
        "dbc"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["dbc"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_dbc::LANGUAGE.into()
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
        let text = unquote(&node_text(source, name)).to_string();
        (!text.is_empty()).then_some(text)
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        dbc_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        dbc_config()
    }
}

/// Convenience registry containing only DBC support.
#[must_use]
pub fn dbc_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(DbcLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real DBC grammar and return every
    /// `(node_kind, extract_name)` where a name is produced.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = DbcLanguage;
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

    // The grammar mandates the canonical DBC section order: VERSION, NS_,
    // BS_, BU_, then VAL_TABLE_ entries BEFORE the BO_ messages.
    const SAMPLE: &str = "VERSION \"1.0\"\n\nNS_ :\n\nBS_:\n\nBU_: ECU1 ECU2\n\nVAL_TABLE_ GearTable 0 \"Neutral\" 1 \"First\" ;\n\nBO_ 256 EngineData: 8 ECU1\n SG_ EngineSpeed : 0|16@1+ (0.25,0) [0|16383.75] \"rpm\" ECU2\n SG_ EngineTemp : 16|8@1+ (1,-40) [-40|215] \"degC\" ECU2\n";

    #[test]
    fn embedded_config_is_valid() {
        let cfg = dbc_config();
        assert!(cfg.kind_map_lookup("message").is_some());
        assert!(cfg.kind_map_lookup("signal").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = DbcLanguage;
        assert_eq!(lang.map_kind("message"), Some("object"));
        assert_eq!(lang.map_kind("signal"), Some("field"));
        assert_eq!(lang.map_kind("value_table"), Some("enum"));
        assert_eq!(lang.map_kind("comment"), Some("comment"));
        assert_eq!(lang.map_kind("baudrate"), None);
    }

    #[test]
    fn registry_resolves_dbc_extension() {
        let registry = dbc_registry();
        let lang = registry.language_for_path(std::path::Path::new("powertrain.dbc"));
        assert!(lang.is_some(), "no language for powertrain.dbc");
        assert_eq!(lang.unwrap().name(), "dbc");
    }

    #[test]
    fn messages_named_by_message_name() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("message".to_string(), "EngineData".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn signals_named_by_signal_name() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("signal".to_string(), "EngineSpeed".to_string())),
            "names: {got:?}"
        );
        assert!(
            got.contains(&("signal".to_string(), "EngineTemp".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn value_tables_named_by_table_name() {
        let got = names(SAMPLE);
        assert!(
            got.contains(&("value_table".to_string(), "GearTable".to_string())),
            "names: {got:?}"
        );
    }

    #[test]
    fn attribute_names_are_unquoted() {
        let src = "VERSION \"1.0\"\n\nNS_ :\n\nBS_:\n\nBU_: ECU1\n\nBA_DEF_ BO_ \"GenMsgCycleTime\" INT 0 65535;\n";
        let got = names(src);
        assert!(
            got.contains(&(
                "attribute_definition".to_string(),
                "GenMsgCycleTime".to_string()
            )),
            "names: {got:?}"
        );
    }
}
