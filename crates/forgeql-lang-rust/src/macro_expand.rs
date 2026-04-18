//! Rust macro expander for the two-pass macro-expansion pipeline.
//!
//! [`RustMacroExpander`] implements [`MacroExpander`] for `macro_rules!`
//! macros in the Rust tree-sitter grammar.
//!
//! ## Best-effort expansion
//!
//! `macro_rules!` can contain multiple match arms with complex patterns.
//! This implementation handles the common single-arm case:
//!
//! 1. The first arm's pattern is scanned for metavariable names (`$ident`).
//! 2. The first arm's template is used as the expansion body.
//! 3. Metavariables are substituted with the call-site arguments using
//!    whole-word replacement (so `$a` does not accidentally replace `$abc`).
//!
//! Multi-arm macros, repetition patterns (`$($x:expr),*`), and other
//! advanced features are handled on a best-effort basis — the enricher
//! simply records `None`/empty when expansion cannot be performed.

use std::borrow::Cow;

use forgeql_core::ast::lang::{LanguageConfig, MacroDef, MacroExpander};

// -----------------------------------------------------------------------
// RustMacroExpander
/// [`MacroExpander`] implementation for Rust `macro_rules!` macros.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct RustMacroExpander;

impl MacroExpander for RustMacroExpander {
    /// Extract a macro definition from a `macro_definition` AST node.
    ///
    /// Returns `None` when `node` is not a `macro_definition`.
    fn extract_def(
        &self,
        node: tree_sitter::Node<'_>,
        source: &[u8],
        config: &LanguageConfig,
    ) -> Option<MacroDef> {
        let kind = node.kind();
        if !config.macro_def_kinds().iter().any(|k| k == kind) {
            return None;
        }

        // Name is in the "name" field (the identifier after `macro_rules!`).
        let name_node = node.child_by_field_name("name")?;
        let name = node_text(source, name_node);
        if name.is_empty() {
            return None;
        }

        // Find the first `macro_rule` child.
        let first_rule = walk_children(node).find(|n| n.kind() == "macro_rule");

        let (params, body) = first_rule.map_or_else(
            || (None, String::new()),
            |rule| {
                // In tree-sitter-rust the two arms of `=>` are named fields
                // "left" (pattern) and "right" (template).
                let params = rule
                    .child_by_field_name("left")
                    .map(|pattern| extract_metavariables(&node_text(source, pattern)));

                let body = rule
                    .child_by_field_name("right")
                    .map_or_else(String::new, |tmpl| {
                        strip_outer_braces(&node_text(source, tmpl))
                    });

                (params, body)
            },
        );

        Some(MacroDef {
            name,
            params,
            body,
            // `file` is set by the caller in `collect_macro_defs_for_file`.
            file: std::path::PathBuf::new(),
            line: u32::try_from(node.start_position().row)
                .unwrap_or(u32::MAX)
                .saturating_add(1),
            guard_group_id: None,
            guard_branch: None,
        })
    }

    /// Extract argument texts from a `macro_invocation` node.
    ///
    /// In tree-sitter-rust, the arguments live in the `token_tree` child
    /// (the last child, after `!`).  Arguments are split on top-level commas.
    fn extract_args(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
        // Find the token_tree child (last child, after `!`).
        let token_tree = walk_children(node)
            .filter(|n| n.kind() == "token_tree")
            .last();

        let Some(tt) = token_tree else {
            return Vec::new();
        };

        let text = node_text(source, tt);
        let inner = strip_outer_delimiters(&text);
        if inner.trim().is_empty() {
            Vec::new()
        } else {
            split_top_level_commas(inner)
        }
    }

    /// Substitute metavariable names (`$param`) with argument values in `body`.
    ///
    /// Uses whole-word replacement: `$a` does not replace inside `$abc`
    /// because `c` is word-adjacent.
    fn substitute(&self, body: &str, params: &[String], args: &[String]) -> String {
        let mut result = body.to_owned();
        for (param, arg) in params.iter().zip(args.iter()) {
            result = replace_whole_word(&result, param, arg);
        }
        result
    }

    /// Wrap expanded text so it can be re-parsed as a standalone statement.
    fn wrap_for_reparse<'a>(&self, expanded: &'a str) -> Cow<'a, str> {
        let trimmed = expanded.trim_end();
        if trimmed.ends_with(';') || trimmed.ends_with('}') || trimmed.is_empty() {
            Cow::Borrowed(expanded)
        } else {
            Cow::Owned(format!("{trimmed};"))
        }
    }
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

use forgeql_core::ast::lang::node_text;

/// Iterate non-recursive (direct) children of `node`.
fn walk_children(node: tree_sitter::Node<'_>) -> impl Iterator<Item = tree_sitter::Node<'_>> {
    let count = node.child_count();
    (0..count).filter_map(move |i| node.child(i))
}

/// Extract all `$ident` metavariable names from a macro pattern text.
///
/// For example, `($a:expr, $b:expr)` → `["$a", "$b"]`.
fn extract_metavariables(text: &str) -> Vec<String> {
    let mut params: Vec<String> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' && chars.peek().is_some_and(|c| c.is_alphabetic() || *c == '_') {
            let mut name = String::from("$");
            while chars
                .peek()
                .is_some_and(|c| c.is_alphanumeric() || *c == '_')
            {
                if let Some(nc) = chars.next() {
                    name.push(nc);
                }
            }
            if !params.contains(&name) {
                params.push(name);
            }
        }
    }
    params
}

/// Strip outer `{ }` braces from a string, trimming whitespace inside.
fn strip_outer_braces(text: &str) -> String {
    let t = text.trim();
    if t.starts_with('{') && t.ends_with('}') {
        t[1..t.len() - 1].trim().to_owned()
    } else {
        t.to_owned()
    }
}

/// Strip outer `( )`, `[ ]`, or `{ }` delimiter pairs from the text.
fn strip_outer_delimiters(text: &str) -> &str {
    let t = text.trim();
    if (t.starts_with('(') && t.ends_with(')'))
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('{') && t.ends_with('}'))
    {
        &t[1..t.len() - 1]
    } else {
        t
    }
}

/// Split `text` by top-level commas (ignoring commas inside nested delimiters).
fn split_top_level_commas(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut depth: i32 = 0;
    let mut current = String::new();
    for ch in text.chars() {
        match ch {
            '(' | '[' | '{' => {
                depth += 1;
                current.push(ch);
            }
            ')' | ']' | '}' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = current.trim().to_owned();
                if !trimmed.is_empty() {
                    result.push(trimmed);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim().to_owned();
    if !trimmed.is_empty() {
        result.push(trimmed);
    }
    result
}

/// Replace all whole-word occurrences of `needle` in `haystack` with `replacement`.
fn replace_whole_word(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_owned();
    }
    let mut result = String::with_capacity(haystack.len());
    let mut remaining = haystack;
    while let Some(pos) = remaining.find(needle) {
        let before = &remaining[..pos];
        let after = &remaining[pos + needle.len()..];
        let left_ok = before
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        let right_ok = after
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '_');
        result.push_str(before);
        if left_ok && right_ok {
            result.push_str(replacement);
        } else {
            result.push_str(needle);
        }
        remaining = after;
    }
    result.push_str(remaining);
    result
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::rust_config;

    fn parse_rust(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let source = src.as_bytes().to_vec();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("set language");
        let tree = parser.parse(&source, None).expect("parse");
        (tree, source)
    }

    #[test]
    fn extract_object_like_macro() {
        let src = "macro_rules! PI { () => { 3 }; }\n";
        let (tree, source) = parse_rust(src);
        let expander = RustMacroExpander;
        let config = rust_config();
        let root = tree.root_node();
        let node = root.child(0).unwrap();
        assert_eq!(node.kind(), "macro_definition");
        let def = expander.extract_def(node, &source, config).unwrap();
        assert_eq!(def.name, "PI");
        // Zero-arg macro_rules: params = Some([]) (function-like with 0 params)
        assert_eq!(def.params, Some(vec![]));
        assert!(!def.body.is_empty());
    }

    #[test]
    fn extract_function_like_macro() {
        let src = "macro_rules! add { ($a:expr, $b:expr) => { $a + $b }; }\n";
        let (tree, source) = parse_rust(src);
        let expander = RustMacroExpander;
        let config = rust_config();
        let root = tree.root_node();
        let node = root.child(0).unwrap();
        let def = expander.extract_def(node, &source, config).unwrap();
        assert_eq!(def.name, "add");
        let params = def.params.unwrap();
        assert!(params.contains(&"$a".to_owned()));
        assert!(params.contains(&"$b".to_owned()));
    }

    #[test]
    fn substitute_metavariables() {
        let expander = RustMacroExpander;
        let body = "$a + $b";
        let params = vec!["$a".to_owned(), "$b".to_owned()];
        let args = vec!["x".to_owned(), "y".to_owned()];
        assert_eq!(expander.substitute(body, &params, &args), "x + y");
    }

    #[test]
    fn substitute_no_partial_match() {
        let expander = RustMacroExpander;
        let body = "$abc + $a";
        let params = vec!["$a".to_owned()];
        let args = vec!["x".to_owned()];
        // $abc should NOT be replaced — only $a
        assert_eq!(expander.substitute(body, &params, &args), "$abc + x");
    }

    #[test]
    fn wrap_for_reparse_appends_semicolon() {
        let expander = RustMacroExpander;
        assert_eq!(
            expander.wrap_for_reparse("x + 1"),
            Cow::Owned::<str>("x + 1;".to_owned())
        );
        assert_eq!(expander.wrap_for_reparse("x + 1;"), Cow::Borrowed("x + 1;"));
        assert_eq!(expander.wrap_for_reparse("{ x }"), Cow::Borrowed("{ x }"));
        assert_eq!(expander.wrap_for_reparse(""), Cow::Borrowed(""));
    }

    #[test]
    fn split_args_top_level() {
        let args = split_top_level_commas("a, b, (c, d)");
        assert_eq!(args, vec!["a", "b", "(c, d)"]);
    }

    #[test]
    fn extract_metavariables_basic() {
        let params = extract_metavariables("($a:expr, $b:expr)");
        assert_eq!(params, vec!["$a".to_owned(), "$b".to_owned()]);
    }

    #[test]
    fn extract_metavariables_dedup() {
        let params = extract_metavariables("($a:expr => $a)");
        assert_eq!(params, vec!["$a".to_owned()]);
    }
    #[test]
    fn rust_macro_invocation_node_structure() {
        // Verify tree-sitter-rust produces macro_invocation nodes and that
        // child_by_field_name("macro") returns the macro name.
        let src = "fn main() { println!(\"hello\"); }\n";
        let (tree, source) = parse_rust(src);
        let root = tree.root_node();
        // Walk the tree looking for macro_invocation
        let mut cursor = root.walk();
        let mut found = false;
        loop {
            let node = cursor.node();
            if node.kind() == "macro_invocation" {
                found = true;
                let field_macro = node.child_by_field_name("macro");
                let named0 = node.named_child(0);
                eprintln!(
                    "macro_invocation: field(macro)={:?}, named_child(0)={:?}",
                    field_macro.map(|n| std::str::from_utf8(&source[n.byte_range()]).unwrap_or("")),
                    named0.map(|n| std::str::from_utf8(&source[n.byte_range()]).unwrap_or("")),
                );
                let macro_name = field_macro.map_or("", |n| {
                    std::str::from_utf8(&source[n.byte_range()]).unwrap_or("")
                });
                assert_eq!(macro_name, "println", "macro name via field(\"macro\")");
                break;
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node() == root {
                break;
            }
        }
        assert!(
            found,
            "macro_invocation node not found in AST: {}",
            root.to_sexp()
        );
    }
}
