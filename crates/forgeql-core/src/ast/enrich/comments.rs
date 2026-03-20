/// Comment style detection and documentation presence enrichment.
///
/// Adds to comment rows:
/// - `comment_style`: `"doc_block"` (`/**`), `"doc_line"` (`///`),
///   `"block"` (`/*`), or `"line"` (`//`)
///
/// Adds to function/struct/class/enum rows:
/// - `has_doc`: `"true"` if the previous sibling is a doc comment
use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;

/// Enricher that computes `comment_style` and `has_doc` fields.
pub struct CommentEnricher;

impl NodeEnricher for CommentEnricher {
    fn name(&self) -> &'static str {
        "comments"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let kind = ctx.node.kind();

        // Comment rows: detect style
        if kind == "comment" {
            let text = node_text(ctx.source, ctx.node);
            let style = detect_comment_style(&text);
            drop(fields.insert("comment_style".to_string(), style.to_string()));
            return;
        }

        // Definition rows: check for preceding doc comment
        if matches!(
            kind,
            "function_definition" | "struct_specifier" | "class_specifier" | "enum_specifier"
        ) {
            let has_doc = ctx.node.prev_named_sibling().is_some_and(|sib| {
                if sib.kind() != "comment" {
                    return false;
                }
                let text = node_text(ctx.source, sib);
                text.starts_with("/**") || text.starts_with("///")
            });
            drop(fields.insert("has_doc".to_string(), has_doc.to_string()));
        }
    }
}

/// Classify a comment's style from its source text.
fn detect_comment_style(text: &str) -> &'static str {
    if text.starts_with("/**") {
        "doc_block"
    } else if text.starts_with("///") {
        "doc_line"
    } else if text.starts_with("/*") {
        "block"
    } else {
        "line"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_styles() {
        assert_eq!(detect_comment_style("/** doc */"), "doc_block");
        assert_eq!(detect_comment_style("/// doc line"), "doc_line");
        assert_eq!(detect_comment_style("/* block */"), "block");
        assert_eq!(detect_comment_style("// line"), "line");
    }
}
