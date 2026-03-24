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
        let config = ctx.language_config;

        // Comment rows: detect style
        if config.is_comment_kind(kind) {
            let text = node_text(ctx.source, ctx.node);
            let style = detect_comment_style(&text, config.doc_comment_prefixes);
            drop(fields.insert("comment_style".to_string(), style.to_string()));
            return;
        }

        // Definition rows: check for preceding doc comment
        if config.is_definition_kind(kind) {
            let has_doc = ctx.node.prev_named_sibling().is_some_and(|sib| {
                if !config.is_comment_kind(sib.kind()) {
                    return false;
                }
                let text = node_text(ctx.source, sib);
                config
                    .doc_comment_prefixes
                    .iter()
                    .take_while(|(_, style)| style.starts_with("doc"))
                    .any(|(prefix, _)| text.starts_with(prefix))
            });
            drop(fields.insert("has_doc".to_string(), has_doc.to_string()));
        }
    }
}

/// Classify a comment's style from its source text using the language config.
fn detect_comment_style(text: &str, prefixes: &[(&str, &'static str)]) -> &'static str {
    for &(prefix, style) in prefixes {
        if text.starts_with(prefix) {
            return style;
        }
    }
    "line"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::CPP_CONFIG;

    #[test]
    fn comment_styles() {
        let p = CPP_CONFIG.doc_comment_prefixes;
        assert_eq!(detect_comment_style("/** doc */", p), "doc_block");
        assert_eq!(detect_comment_style("/// doc line", p), "doc_line");
        assert_eq!(detect_comment_style("/* block */", p), "block");
        assert_eq!(detect_comment_style("// line", p), "line");
    }
}
