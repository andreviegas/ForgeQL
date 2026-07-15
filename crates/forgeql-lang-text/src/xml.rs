//! XML language support for ForgeQL.
//!
//! Implements [`LanguageSupport`] for XML using the official `tree-sitter-xml`
//! grammar. One language claims the whole XML family used by automotive
//! toolchains — AUTOSAR `.arxml`, EB tresos `.xdm`/`.epc`/`.epd`, ECU
//! configuration `.ecuc`, diagnostics `.odx` — plus plain `.xml`, so those
//! files become node-addressable and editable by stable `node_id` instead of
//! through a GUI round-trip.
//!
//! The addressable unit is the `element`. Every element is indexed (nested
//! elements each get their own ordinal, exactly like nested `if`/`for`
//! blocks in code), named by a mechanical cascade:
//!
//! 1. an identifier-like attribute — `name`/`id`/`key`/`title`/`alias`,
//!    case-insensitive (tresos: `<d:ctr name="AdcConfigSet">`);
//! 2. a `SHORT-NAME` child element's text (AUTOSAR:
//!    `<ECUC-CONTAINER-VALUE><SHORT-NAME>AdcHwUnit0</SHORT-NAME>…`);
//! 3. the last `/`-segment of a `DEFINITION-REF` child's text (AUTOSAR ECUC
//!    parameter/reference values carry neither attribute nor SHORT-NAME —
//!    their identity is the referenced definition, e.g.
//!    `…/CanIfPublicCfg/CanIfPublicTxBuffering` → `CanIfPublicTxBuffering`);
//! 4. the tag name itself — so anonymous structural wrappers such as
//!    `<CONTAINERS>` or `<ELEMENTS>` stay addressable for INSERT targets.
//!
//! Attributes are NOT indexed as separate nodes: they live on the element's
//! start tag and are edited through the element's handle (token thrift — a
//! 100 MB arxml would otherwise triple its row count for no addressing gain).

#![allow(clippy::module_name_repetitions, clippy::doc_markdown)]
// Tests use unwrap/expect intentionally — the pedantic lints are for library code.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::sync::{Arc, OnceLock};

use forgeql_core::ast::lang::{LanguageConfig, LanguageRegistry, LanguageSupport, node_text};
use forgeql_core::ast::lang_json::LanguageConfigJson;

/// XML language support for ForgeQL.
pub struct XmlLanguage;

/// Static configuration for XML.
static XML_CONFIG: OnceLock<LanguageConfig> = OnceLock::new();

/// Attribute names, in priority order, that identify their element
/// (compared case-insensitively — tresos uses `name`, other dialects `NAME`).
const IDENTIFIER_ATTRS: &[&str] = &["name", "id", "key", "title", "alias"];

/// Child-element tag whose text names its parent container (AUTOSAR).
const SHORT_NAME_TAG: &str = "SHORT-NAME";

/// Child-element tag holding an ECUC definition reference (AUTOSAR
/// configuration values). Its text is a slash-separated definition path;
/// the last segment names the parameter/reference value.
const DEFINITION_REF_TAG: &str = "DEFINITION-REF";

/// Start-tag node kinds (`<a …>` and the self-closing `<a …/>`).
const TAG_KINDS: &[&str] = &["STag", "EmptyElemTag"];

/// Returns the static XML language configuration, loaded from
/// `config/xml.json` (embedded at compile time).
///
/// # Panics
///
/// Panics if the embedded `xml.json` is malformed (should never happen — the
/// file is validated at test time).
#[expect(
    clippy::expect_used,
    reason = "embedded JSON is validated at test time; a parse failure is a programming error"
)]
pub fn xml_config() -> &'static LanguageConfig {
    XML_CONFIG.get_or_init(|| {
        let json_bytes = include_bytes!("../config/xml.json");
        let json_config = LanguageConfigJson::from_json_bytes(json_bytes)
            .expect("embedded xml.json must be valid");
        json_config.into_language_config()
    })
}

/// Strip surrounding single or double quotes from an attribute value.
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

/// The start tag (`STag` or `EmptyElemTag`) of an element, if any.
fn start_tag(element: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut cursor = element.walk();
    element
        .named_children(&mut cursor)
        .find(|child| TAG_KINDS.contains(&child.kind()))
}

/// The tag name of a start tag (its first `Name` child).
fn tag_name(tag: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = tag.walk();
    let name = tag
        .named_children(&mut cursor)
        .find(|child| child.kind() == "Name")?;
    let text = node_text(source, name).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// The value of the first identifier-like attribute on a start tag, if any.
///
/// Attributes are scanned in document order; the first whose name matches an
/// entry of [`IDENTIFIER_ATTRS`] (case-insensitive) wins.
fn attr_identifier(tag: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = tag.walk();
    for attr in tag.named_children(&mut cursor) {
        if attr.kind() != "Attribute" {
            continue;
        }
        let mut attr_cursor = attr.walk();
        let mut name: Option<String> = None;
        let mut value: Option<String> = None;
        for part in attr.named_children(&mut attr_cursor) {
            match part.kind() {
                "Name" => name = Some(node_text(source, part).trim().to_string()),
                "AttValue" => value = Some(unquote(&node_text(source, part)).to_string()),
                _ => {}
            }
        }
        let Some(name) = name else { continue };
        if !IDENTIFIER_ATTRS
            .iter()
            .any(|id| name.eq_ignore_ascii_case(id))
        {
            continue;
        }
        if let Some(value) = value
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

/// The trimmed text of the first child element whose tag equals `tag`
/// (case-insensitive), if `element` has one.
fn child_element_text(element: tree_sitter::Node<'_>, tag: &str, source: &[u8]) -> Option<String> {
    let mut cursor = element.walk();
    let content = element
        .named_children(&mut cursor)
        .find(|child| child.kind() == "content")?;
    let mut content_cursor = content.walk();
    for child in content.named_children(&mut content_cursor) {
        if child.kind() != "element" {
            continue;
        }
        let Some(child_tag) = start_tag(child) else {
            continue;
        };
        let matches =
            tag_name(child_tag, source).is_some_and(|name| name.eq_ignore_ascii_case(tag));
        if !matches {
            continue;
        }
        let mut child_cursor = child.walk();
        let text = child
            .named_children(&mut child_cursor)
            .find(|part| part.kind() == "content")
            .map(|part| node_text(source, part).trim().to_string())?;
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// The text of a `SHORT-NAME` child element, if `element` has one (AUTOSAR).
fn short_name_child(element: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    child_element_text(element, SHORT_NAME_TAG, source)
}

/// The last `/`-segment of a `DEFINITION-REF` child's text, if `element`
/// has one (AUTOSAR ECUC configuration values).
///
/// Parameter and reference values (`ECUC-NUMERICAL-PARAM-VALUE`,
/// `ECUC-TEXTUAL-PARAM-VALUE`, `ECUC-REFERENCE-VALUE`, …) carry no
/// SHORT-NAME and no identifying attribute; their identity is the referenced
/// definition path, e.g.
/// `/AUTOSAR/EcucDefs/CanIf/CanIfPublicCfg/CanIfPublicTxBuffering` —
/// whose last segment is the parameter name a user searches for.
fn definition_ref_last_segment(element: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let text = child_element_text(element, DEFINITION_REF_TAG, source)?;
    let segment = text.rsplit('/').next()?.trim();
    (!segment.is_empty()).then(|| segment.to_string())
}

impl LanguageSupport for XmlLanguage {
    fn name(&self) -> &'static str {
        "xml"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["xml", "arxml", "xdm", "epc", "epd", "ecuc", "odx"]
    }

    fn tree_sitter_language(&self) -> tree_sitter::Language {
        tree_sitter_xml::LANGUAGE_XML.into()
    }

    fn extract_name(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
        if node.kind() != "element" {
            return None;
        }
        let tag = start_tag(node)?;
        attr_identifier(tag, source)
            .or_else(|| short_name_child(node, source))
            .or_else(|| definition_ref_last_segment(node, source))
            .or_else(|| tag_name(tag, source))
    }

    fn map_kind(&self, raw_kind: &str) -> Option<&'static str> {
        xml_config().kind_map_lookup(raw_kind)
    }

    fn config(&self) -> &'static LanguageConfig {
        xml_config()
    }

    fn validate_source(
        &self,
        source: &[u8],
        _path: &std::path::Path,
    ) -> Option<Result<(), String>> {
        // Well-formedness only: balanced tags and valid syntax, not schema/DTD
        // validity. Applies uniformly to every XML dialect (arxml, odx, …).
        let mut reader = quick_xml::Reader::from_reader(source);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) => return Some(Ok(())),
                Ok(_) => buf.clear(),
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }
}

/// Convenience registry containing only XML support.
#[must_use]
pub fn xml_registry() -> LanguageRegistry {
    LanguageRegistry::new(vec![Arc::new(XmlLanguage)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse `src` with the real XML grammar and return every
    /// `(node_kind, extract_name)` where a name is produced — the end-to-end
    /// check that our node-kind assumptions match the grammar.
    fn names(src: &str) -> Vec<(String, String)> {
        let lang = XmlLanguage;
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

    fn just_names(src: &str) -> Vec<String> {
        names(src).into_iter().map(|(_, n)| n).collect()
    }

    #[test]
    fn embedded_config_is_valid() {
        let cfg = xml_config();
        assert!(cfg.kind_map_lookup("element").is_some());
        assert!(cfg.kind_map_lookup("Comment").is_some());
    }

    #[test]
    fn map_kind_covers_structural_kinds() {
        let lang = XmlLanguage;
        assert_eq!(lang.map_kind("element"), Some("object"));
        assert_eq!(lang.map_kind("Comment"), Some("comment"));
        assert_eq!(lang.map_kind("CharData"), None);
    }

    #[test]
    fn registry_resolves_xml_family_extensions() {
        let registry = xml_registry();
        for file in [
            "config.xml",
            "Adc_ecuc.arxml",
            "Adc.xdm",
            "project.epc",
            "project.epd",
            "Adc.ecuc",
            "diag.odx",
        ] {
            let lang = registry.language_for_path(std::path::Path::new(file));
            assert!(lang.is_some(), "no language for {file}");
            assert_eq!(lang.unwrap().name(), "xml");
        }
    }

    #[test]
    fn tresos_elements_named_by_name_attribute() {
        // EB tresos .xdm style: containers and vars carry a `name` attribute,
        // nested containers each get their own name (→ their own node_id).
        let src = r#"<d:ctr name="AdcConfigSet" type="MAP">
  <d:ctr name="AdcHwUnit_0" type="IDENTIFIABLE">
    <d:var name="AdcPriority" type="INTEGER" value="0"/>
  </d:ctr>
</d:ctr>"#;
        let got = just_names(src);
        assert!(got.contains(&"AdcConfigSet".to_string()), "names: {got:?}");
        assert!(got.contains(&"AdcHwUnit_0".to_string()), "names: {got:?}");
        assert!(got.contains(&"AdcPriority".to_string()), "names: {got:?}");
    }

    #[test]
    fn autosar_containers_named_by_short_name_child() {
        // AUTOSAR .arxml style: identity is a SHORT-NAME child element.
        let src = "<ECUC-CONTAINER-VALUE>\
                     <SHORT-NAME>AdcHwUnit0</SHORT-NAME>\
                     <DEFINITION-REF>/AUTOSAR/Adc/AdcHwUnit</DEFINITION-REF>\
                   </ECUC-CONTAINER-VALUE>";
        let got = just_names(src);
        assert!(got.contains(&"AdcHwUnit0".to_string()), "names: {got:?}");
        // The SHORT-NAME element itself falls back to its tag name.
        assert!(got.contains(&"SHORT-NAME".to_string()), "names: {got:?}");
    }

    #[test]
    fn anonymous_wrappers_fall_back_to_tag_name() {
        // Structural wrappers stay addressable so INSERT INTO them works.
        let src = "<CONTAINERS><PARAMETER-VALUES></PARAMETER-VALUES></CONTAINERS>";
        let got = just_names(src);
        assert!(got.contains(&"CONTAINERS".to_string()), "names: {got:?}");
        assert!(
            got.contains(&"PARAMETER-VALUES".to_string()),
            "names: {got:?}"
        );
    }

    #[test]
    fn name_attribute_wins_over_short_name_and_tag() {
        let src = r#"<A name="attr-wins"><SHORT-NAME>short</SHORT-NAME></A>"#;
        let got = names(src);
        let attr_won = got
            .iter()
            .filter(|(kind, _)| kind == "element")
            .any(|(_, n)| n == "attr-wins");
        assert!(attr_won, "names: {got:?}");
    }

    #[test]
    fn identifier_attributes_match_case_insensitively() {
        let src = r#"<ELEM NAME="UpperCased"/>"#;
        let got = just_names(src);
        assert!(got.contains(&"UpperCased".to_string()), "names: {got:?}");
    }

    #[test]
    fn single_quoted_attribute_values_are_unquoted() {
        let src = "<node id='n42'/>";
        let got = just_names(src);
        assert!(got.contains(&"n42".to_string()), "names: {got:?}");
    }

    #[test]
    fn ecuc_param_values_are_named_by_definition_ref_last_segment() {
        // AUTOSAR ECUC values carry neither an identifying attribute nor a
        // SHORT-NAME; their DEFINITION-REF's last path segment is the name a
        // user searches for.
        let src = r#"<PARAMETER-VALUES>
  <ECUC-NUMERICAL-PARAM-VALUE>
    <DEFINITION-REF DEST="ECUC-BOOLEAN-PARAM-DEF">/AUTOSAR/EcucDefs/CanIf/CanIfPublicCfg/CanIfPublicTxBuffering</DEFINITION-REF>
    <VALUE>true</VALUE>
  </ECUC-NUMERICAL-PARAM-VALUE>
  <ECUC-REFERENCE-VALUE>
    <DEFINITION-REF DEST="ECUC-REFERENCE-DEF">/AUTOSAR/EcucDefs/CanIf/CanIfInitCfg/CanIfBufferCfg/CanIfBufferHthIdRef</DEFINITION-REF>
    <VALUE-REF DEST="ECUC-CONTAINER-VALUE">/ActiveEcuC/CanIf/CanIfHthCfg_0</VALUE-REF>
  </ECUC-REFERENCE-VALUE>
</PARAMETER-VALUES>"#;
        let names = just_names(src);
        assert!(
            names.contains(&"CanIfPublicTxBuffering".to_string()),
            "param value must be named by DEFINITION-REF last segment: {names:?}"
        );
        assert!(
            names.contains(&"CanIfBufferHthIdRef".to_string()),
            "reference value must be named by DEFINITION-REF last segment: {names:?}"
        );
        // The DEFINITION-REF element itself keeps its tag name.
        assert!(names.contains(&"DEFINITION-REF".to_string()));
    }

    #[test]
    fn short_name_still_wins_over_definition_ref() {
        // Containers carry BOTH a SHORT-NAME and a DEFINITION-REF; the
        // SHORT-NAME (the instance name) must win over the definition name.
        let src = r#"<ECUC-CONTAINER-VALUE>
  <SHORT-NAME>CanIfBufferCfg_0</SHORT-NAME>
  <DEFINITION-REF DEST="ECUC-PARAM-CONF-CONTAINER-DEF">/AUTOSAR/EcucDefs/CanIf/CanIfInitCfg/CanIfBufferCfg</DEFINITION-REF>
</ECUC-CONTAINER-VALUE>"#;
        let names = just_names(src);
        assert!(names.contains(&"CanIfBufferCfg_0".to_string()));
        assert!(
            !names.contains(&"CanIfBufferCfg".to_string()),
            "SHORT-NAME must take priority over DEFINITION-REF: {names:?}"
        );
    }
}
