//! Structured-text language support for ForgeQL.
//!
//! One crate groups every text/data/markup format so that adding a new
//! extension is always the same three-step recipe in the same place:
//! a `config/<lang>.json` kind map, a `src/<lang>.rs` module implementing
//! [`LanguageSupport`](forgeql_core::ast::lang::LanguageSupport) with its
//! naming rules, and a line in [`text_languages`].
//!
//! Formats covered today:
//!
//! | module     | extensions                                        |
//! |------------|---------------------------------------------------|
//! | [`json`]   | `json`                                            |
//! | [`yaml`]   | `yaml`, `yml`                                     |
//! | [`toml`]   | `toml`, `lock` (Cargo.lock)                       |
//! | [`markdown`] | `md`, `markdown`                                |
//! | [`xml`]    | `xml`, `arxml`, `xdm`, `epc`, `epd`, `ecuc`, `odx`|
//! | [`dbc`]    | `dbc` (Vector CAN database)                       |
//! | [`ini`]    | `ini`, `cfg`, `.editorconfig`, `.gitconfig`       |
//! | [`just`]   | `just`, `justfile` / `.justfile` / `Justfile`     |
//!
//! All of them map onto the same generic addressable kinds (`object`,
//! `pair`, `array`, `section`, `heading`, `comment`, …), so the core engine
//! stays language-agnostic while every nested entry of a configuration or
//! data file gets a stable, nested `node_id`.

#![allow(clippy::doc_markdown)]

pub mod dbc;
pub mod ini;
pub mod json;
pub mod just;
pub mod markdown;
pub mod toml;
pub mod xml;
pub mod yaml;

use std::sync::Arc;

use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};

pub use dbc::DbcLanguage;
pub use ini::IniLanguage;
pub use json::JsonLanguage;
pub use just::JustLanguage;
pub use markdown::MarkdownLanguage;
pub use toml::TomlLanguage;
pub use xml::XmlLanguage;
pub use yaml::YamlLanguage;

/// Every structured-text language this crate provides, ready to splice into
/// a [`LanguageRegistry`] alongside the code languages.
#[must_use]
pub fn text_languages() -> Vec<Arc<dyn LanguageSupport>> {
    vec![
        Arc::new(DbcLanguage),
        Arc::new(IniLanguage),
        Arc::new(JsonLanguage),
        Arc::new(JustLanguage),
        Arc::new(MarkdownLanguage),
        Arc::new(TomlLanguage),
        Arc::new(XmlLanguage),
        Arc::new(YamlLanguage),
    ]
}

/// Convenience registry containing only the structured-text languages.
#[must_use]
pub fn text_registry() -> LanguageRegistry {
    LanguageRegistry::new(text_languages())
}
