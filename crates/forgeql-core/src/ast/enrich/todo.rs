/// Todo/fixme enrichment — detects annotation-style comments inside a
/// function: its body, and any comment attached directly to the function node.
///
/// The scanned region is the `body` subtree, if the grammar gives the function
/// one, PLUS any comment that is a direct child of the function node. The
/// second half is not decoration: in an indentation-delimited grammar the body
/// block does not open until the first statement, so a comment written as the
/// first line of the body is parsed as a sibling of the block rather than a
/// child of it, and a body-only walk reports no marker for a function whose
/// first line is one. It also brings in a comment written between the signature
/// and the body, which sits in the same place in a brace-delimited grammar and
/// was unseen for the same reason. A comment on the LAST line of a body needs
/// nothing: the block has opened by then, so it was always inside.
///
/// Three things are outside the region, and each is a tree fact rather than a
/// rule written here:
///
/// 1. A comment PRECEDING the function is its doc comment. Extras attach to the
///    enclosing node, so it is a sibling of the function.
/// 2. A comment between a DECORATOR and the definition it decorates is a child
///    of the wrapper, hence a sibling of the definition that owns the row —
///    even though the row's rendered span folds back to the decorator, so such
///    a comment sits inside the span an agent reads.
/// 3. Any comment style the language does not declare. "Comment" here means the
///    one raw node kind named by the config ([`LanguageConfig::is_comment_kind`]
///    is an equality), so where a grammar splits the styles and the config names
///    one — Rust's `line_comment` against its `block_comment` — the other is not
///    scanned in any position. That last one is a gap this walk does not close.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `has_todo`: `"true"` if any TODO/FIXME/HACK/XXX marker was found.
/// - `todo_count`: total number of marker occurrences.
/// - `todo_tags`: comma-separated, sorted list of unique tags found
///   (e.g. `"FIXME,TODO"`).
///
/// **Language-agnostic:** uses `function_raw_kinds` and
/// `comment_raw_kind` from [`LanguageConfig`].
use std::collections::{BTreeSet, HashMap};

use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// Recognised marker tags (case-insensitive match).
const MARKERS: &[&str] = &["TODO", "FIXME", "HACK", "XXX"];

/// Enricher for TODO / FIXME / HACK / XXX detection.
pub struct TodoEnricher;

impl NodeEnricher for TodoEnricher {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let config = ctx.language_config;
        if !config.is_function_kind(ctx.node.kind()) {
            return;
        }

        if !config.has_comment() {
            return;
        }

        let mut count = 0u32;
        let mut tags = BTreeSet::new();

        // The function's own comment children carry the two positions a
        // body-only walk cannot reach; the module doc says which and why. The
        // body is optional rather than an early return, so the region stays
        // "the body if there is one, plus the function's own comment children"
        // even for a function kind whose grammar declares no body.
        let mut cursor = ctx.node.walk();
        for child in ctx.node.children(&mut cursor) {
            if config.is_comment_kind(child.kind()) {
                collect_todos(child, ctx.source, config, &mut count, &mut tags);
            }
        }

        if let Some(body) = ctx.node.child_by_field_name("body") {
            collect_todos(body, ctx.source, config, &mut count, &mut tags);
        }
        if count > 0 {
            drop(fields.insert("has_todo".into(), "true".into()));
            drop(fields.insert("todo_count".into(), count.to_string()));
            let joined: Vec<&str> = tags.iter().map(String::as_str).collect();
            drop(fields.insert("todo_tags".into(), joined.join(",")));
        }
    }
}

/// Walk `node` looking for comments that contain marker tags.
fn collect_todos(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    count: &mut u32,
    tags: &mut BTreeSet<String>,
) {
    if config.is_comment_kind(node.kind()) {
        let text = node_text(source, node);
        let upper = text.to_ascii_uppercase();
        for marker in MARKERS {
            // Count all non-overlapping occurrences of the marker.
            let hits = count_marker_occurrences(&upper, marker);
            if hits > 0 {
                *count += hits;
                let _ = tags.insert((*marker).to_string());
            }
        }
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_todos(child, source, config, count, tags);
    }
}

/// Count non-overlapping, word-boundary-aware occurrences of `marker`
/// (upper-cased) in `upper` (upper-cased text).
fn count_marker_occurrences(upper: &str, marker: &str) -> u32 {
    let bytes = upper.as_bytes();
    let m_bytes = marker.as_bytes();
    let m_len = m_bytes.len();
    let mut hits = 0u32;
    let mut start = 0usize;
    while let Some(pos) = upper[start..].find(marker) {
        let abs = start + pos;
        // Check left boundary: must be start-of-string or non-alphanumeric.
        let left_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        // Check right boundary.
        let right = abs + m_len;
        let right_ok = right >= bytes.len() || !bytes[right].is_ascii_alphanumeric();
        if left_ok && right_ok {
            hits += 1;
        }
        start = abs + m_len;
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_count_basic() {
        assert_eq!(count_marker_occurrences("// TODO: FIX THIS", "TODO"), 1);
        assert_eq!(count_marker_occurrences("// TODO: FIX THIS", "FIXME"), 0);
        assert_eq!(count_marker_occurrences("// TODO TODO", "TODO"), 2);
    }

    #[test]
    fn marker_word_boundary() {
        // "FOOTODO" should NOT match TODO
        assert_eq!(count_marker_occurrences("FOOTODO", "TODO"), 0);
        // "TODO:" should match
        assert_eq!(count_marker_occurrences("TODO:", "TODO"), 1);
        // "TODO(user)" should match
        assert_eq!(count_marker_occurrences("TODO(USER)", "TODO"), 1);
    }
}
