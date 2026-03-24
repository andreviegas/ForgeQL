/// Member declaration enrichment.
///
/// For `field_declaration` nodes, adds:
///
/// - `body_symbol`: (method declarations only) the qualified name
///   (`ClassName::method`) under which the corresponding function definition
///   is typically indexed.
/// - `member_kind`: `"method"` if the field contains a `function_declarator`,
///   otherwise `"field"`.
/// - `owner_kind`: the raw tree-sitter kind of the enclosing type node
///   (e.g. `"class_specifier"`, `"struct_specifier"`).
///
/// `body_symbol` allows consumers like `show_body` to resolve a bare member
/// name (e.g. `loadSignalCode`) to its out-of-line definition
/// (`SignalSequencer::loadSignalCode`) without any language-specific logic
/// at query time.
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;

/// Enricher that links member function declarations to their definitions.
pub struct MemberEnricher;

impl NodeEnricher for MemberEnricher {
    fn name(&self) -> &'static str {
        "member"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let config = ctx.language_config;
        if !config.is_field_kind(ctx.node.kind()) {
            return;
        }

        let is_method = ctx
            .node
            .child_by_field_name(config.declarator_field())
            .is_some_and(|d| has_descendant_kind(d, config.function_declarator()));

        // member_kind: method vs field
        let member_kind = if is_method { "method" } else { "field" };
        drop(fields.insert("member_kind".to_string(), member_kind.to_string()));

        // owner_kind: raw kind of enclosing type node
        if let Some(owner) = enclosing_type_node(ctx.node, config.type_kinds()) {
            drop(fields.insert("owner_kind".to_string(), owner.kind().to_string()));
        }

        // body_symbol: only for method declarations
        if !is_method {
            return;
        }

        // Walk up to find the enclosing class/struct.
        if let Some(class_name) = enclosing_type_name(ctx.node, ctx.source, config.type_kinds()) {
            drop(fields.insert(
                "body_symbol".to_string(),
                format!("{class_name}{}{name}", config.scope_sep()),
            ));
        }
    }
}

/// Check whether `node` or any descendant has kind `target`.
fn has_descendant_kind(node: tree_sitter::Node<'_>, target: &str) -> bool {
    if node.kind() == target {
        return true;
    }
    for i in 0..node.named_child_count() {
        if let Some(child) = node.named_child(i)
            && has_descendant_kind(child, target)
        {
            return true;
        }
    }
    false
}

/// Walk up the parent chain to find an enclosing type node.
fn enclosing_type_node<'a>(
    node: tree_sitter::Node<'a>,
    type_raw_kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if type_raw_kinds.contains(&parent.kind()) {
            return Some(parent);
        }
        current = parent.parent();
    }
    None
}

/// Walk up the parent chain to find an enclosing type node and return its name.
fn enclosing_type_name(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    type_raw_kinds: &[&str],
) -> Option<String> {
    let mut current = node.parent();
    while let Some(parent) = current {
        if type_raw_kinds.contains(&parent.kind()) {
            return parent
                .child_by_field_name("name")
                .map(|n| node_text(source, n))
                .filter(|s| !s.is_empty());
        }
        current = parent.parent();
    }
    None
}
