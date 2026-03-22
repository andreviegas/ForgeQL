/// Member declaration enrichment.
///
/// For `field_declaration` nodes that contain a `function_declarator`
/// (i.e. method prototypes inside a class/struct body), adds:
///
/// - `body_symbol`: the qualified name (`ClassName::method`) under which
///   the corresponding function definition is typically indexed.
///
/// This allows consumers like `show_body` to resolve a bare member name
/// (e.g. `loadSignalCode`) to its out-of-line definition
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
        if !config.field_raw_kinds.contains(&ctx.node.kind()) {
            return;
        }

        // Only method declarations — must contain a function_declarator.
        let has_func_decl = ctx
            .node
            .child_by_field_name("declarator")
            .is_some_and(|d| has_descendant_kind(d, "function_declarator"));
        if !has_func_decl {
            return;
        }

        // Walk up to find the enclosing class/struct.
        if let Some(class_name) = enclosing_type_name(ctx.node, ctx.source, config.type_raw_kinds) {
            drop(fields.insert(
                "body_symbol".to_string(),
                format!("{class_name}{}{name}", config.scope_separator),
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
