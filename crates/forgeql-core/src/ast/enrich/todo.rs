/// Todo/fixme enrichment — detects annotation-style comments inside
/// function bodies.
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

        let Some(body) = ctx.node.child_by_field_name("body") else {
            return;
        };

        let mut count = 0u32;
        let mut tags = BTreeSet::new();
        collect_todos(body, ctx.source, config, &mut count, &mut tags);

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
