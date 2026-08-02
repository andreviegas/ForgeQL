//! Kconfig language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for Linux/Zephyr `Kconfig` files using
//! `tree-sitter-kconfig`. The extension list carries the bare name `kconfig`,
//! which the registry's file-name fallback resolves for a file called
//! `Kconfig` with no extension at all.
//!
//! `config X` and `menuconfig X` index as `variable` rows named `X` — the
//! definition site of a build flag, which had no addressable row before. Every
//! `symbol` is a usage site, so `depends on X`, `select X` and `if X` all
//! answer `FIND usages OF 'X'` without further configuration.
//!
//! Only `config` and `menuconfig` are named. A row is emitted for a *named*
//! node, so a `kind_map` entry alone is inert — `NAME_CHILD` is what decides
//! what exists. `if` is left out of both: its condition is a bare `symbol`,
//! so naming the guard after it would report a flag's own name twice in the
//! file that defines it, and `if` nests the entries it guards, so it would
//! re-parent them. Left alone it still contributes its usage site.

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// Kconfig language support for ForgeQL.
pub struct KconfigLanguage;

/// Static configuration for Kconfig.
static KCONFIG_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// `(container kind, name-child kind)` pairs: each indexed construct is named
/// by the text of its first child of the given kind.
const NAME_CHILD: &[(&str, &str)] = &[("config", "name"), ("menuconfig", "name")];

/// Returns the static Kconfig language configuration, loaded from
/// `config/kconfig.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `kconfig.json` is malformed (should never happen —
/// the file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn kconfig_config() -> &'static LanguageConfig {
    KCONFIG_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/kconfig.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded kconfig.json must be valid");
        json_config.into_language_config()
    })
}

impl LanguageSupport for KconfigLanguage {
    fn name(&self) -> &'static str {
        "kconfig"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["kconfig"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_kconfig::LANGUAGE.into()
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
        let text = node_text(source, name).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        kconfig_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        kconfig_config()
    }
}

/// Convenience registry containing only Kconfig support.
#[must_use]
pub fn kconfig_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(KconfigLanguage)])
}
