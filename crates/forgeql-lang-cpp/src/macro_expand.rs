//! C/C++ macro expander for the two-pass macro-expansion pipeline.
//!
//! [`CppMacroExpander`] implements [`MacroExpander`] for the C++ tree-sitter
//! grammar.  It handles both object-like (`#define FOO 42`) and function-like
//! (`#define MAX(a,b) ((a)>(b)?(a):(b))`) macros.

use std::borrow::Cow;

use forgeql_core::ast::lang::{LanguageConfig, MacroDef, MacroExpander};

// -----------------------------------------------------------------------
// CppMacroExpander
/// [`MacroExpander`] implementation for C/C++.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct CppMacroExpander;
impl MacroExpander for CppMacroExpander {
    /// Extract a macro definition from a `preproc_def` or
    /// `preproc_function_def` AST node.
    ///
    /// Returns `None` when `node` is not a C/C++ macro definition kind.
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

        let name_node = node.child_by_field_name("name")?;
        let name = node_text(source, name_node);
        if name.is_empty() {
            return None;
        }

        // Parameters — only present on preproc_function_def.
        let params = if kind == "preproc_function_def" {
            let params_node = node.child_by_field_name(config.macro_parameters_field());
            Some(params_node.map_or_else(Vec::new, |p| extract_param_names(source, p)))
        } else {
            None
        };

        // Body — the `value` field, joined across continuations.
        let body = node
            .child_by_field_name(config.macro_value_field())
            .map_or_else(String::new, |v| join_continuations(source, v));

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
    /// The tree-sitter C++ grammar wraps arguments inside an
    /// `argument_list` child; we collect the text of each named argument.
    fn extract_args(&self, node: tree_sitter::Node<'_>, source: &[u8]) -> Vec<String> {
        let arg_list = (0..node.named_child_count())
            .filter_map(|i| node.named_child(i))
            .find(|n| n.kind() == "argument_list");

        let Some(arg_list) = arg_list else {
            return Vec::new();
        };

        (0..arg_list.named_child_count())
            .filter_map(|i| arg_list.named_child(i))
            .map(|n| node_text(source, n))
            .collect()
    }

    /// Substitute `params` → `args` inside `body`.
    ///
    /// Uses whole-word replacement to avoid partial-token substitutions.
    fn substitute(&self, body: &str, params: &[String], args: &[String]) -> String {
        let mut result = body.to_owned();
        for (param, arg) in params.iter().zip(args.iter()) {
            result = replace_whole_word(&result, param, arg);
        }
        result
    }

    /// Wrap expanded text so it can be re-parsed as a standalone expression.
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

fn extract_param_names(source: &[u8], params_node: tree_sitter::Node<'_>) -> Vec<String> {
    (0..params_node.named_child_count())
        .filter_map(|i| params_node.named_child(i))
        .filter(|n| n.kind() == "identifier" || n.kind() == "...")
        .map(|n| node_text(source, n))
        .collect()
}

fn join_continuations(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    node_text(source, node)
        .lines()
        .map(|line| line.strip_suffix('\\').unwrap_or(line).trim_end())
        .collect::<Vec<_>>()
        .join(" ")
}

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
    use crate::cpp_config;

    fn parse_cpp(src: &str) -> (tree_sitter::Tree, Vec<u8>) {
        let source = src.as_bytes().to_vec();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("set language");
        let tree = parser.parse(&source, None).expect("parse");
        (tree, source)
    }

    #[test]
    fn extract_object_like_macro() {
        let src = "#define PI 3.14159f\n";
        let (tree, source) = parse_cpp(src);
        let expander = CppMacroExpander;
        let config = cpp_config();
        let root = tree.root_node();
        let node = root.child(0).unwrap();
        assert_eq!(node.kind(), "preproc_def");
        let def = expander.extract_def(node, &source, config).unwrap();
        assert_eq!(def.name, "PI");
        assert!(def.params.is_none());
        assert_eq!(def.body.trim(), "3.14159f");
    }

    #[test]
    fn extract_function_like_macro() {
        let src = "#define MAX(a, b) ((a) > (b) ? (a) : (b))\n";
        let (tree, source) = parse_cpp(src);
        let expander = CppMacroExpander;
        let config = cpp_config();
        let root = tree.root_node();
        let node = root.child(0).unwrap();
        assert_eq!(node.kind(), "preproc_function_def");
        let def = expander.extract_def(node, &source, config).unwrap();
        assert_eq!(def.name, "MAX");
        let params = def.params.as_ref().unwrap();
        assert_eq!(params, &["a", "b"]);
        assert!(def.body.contains("(a) > (b)"));
    }

    #[test]
    fn extract_no_body_macro() {
        let src = "#define NDEBUG\n";
        let (tree, source) = parse_cpp(src);
        let expander = CppMacroExpander;
        let config = cpp_config();
        let root = tree.root_node();
        let node = root.child(0).unwrap();
        let def = expander.extract_def(node, &source, config).unwrap();
        assert_eq!(def.name, "NDEBUG");
        assert!(def.params.is_none());
        assert_eq!(def.body, "");
    }

    #[test]
    fn substitute_function_like() {
        let expander = CppMacroExpander;
        let body = "((a) > (b) ? (a) : (b))";
        let params = vec!["a".to_owned(), "b".to_owned()];
        let args = vec!["x".to_owned(), "y".to_owned()];
        let out = expander.substitute(body, &params, &args);
        assert_eq!(out, "((x) > (y) ? (x) : (y))");
    }

    #[test]
    fn substitute_whole_word_only() {
        let expander = CppMacroExpander;
        let body = "alpha + a";
        let out = expander.substitute(body, &["a".to_owned()], &["Z".to_owned()]);
        assert_eq!(out, "alpha + Z");
    }

    #[test]
    fn wrap_for_reparse_adds_semicolon() {
        let expander = CppMacroExpander;
        let owned: Cow<str> = Cow::Owned("x + 1;".to_owned());
        assert_eq!(expander.wrap_for_reparse("x + 1"), owned);
        assert_eq!(expander.wrap_for_reparse("x + 1;"), Cow::Borrowed("x + 1;"));
        assert_eq!(expander.wrap_for_reparse("{}"), Cow::Borrowed("{}"));
    }

    #[test]
    fn replace_whole_word_no_partial() {
        assert_eq!(replace_whole_word("foobar foo", "foo", "baz"), "foobar baz");
        assert_eq!(
            replace_whole_word("foo_bar foo", "foo", "baz"),
            "foo_bar baz"
        );
    }
    #[test]
    fn call_expr_macro_structure() {
        // Confirm: tree-sitter-cpp parses `MACRO(args);` as call_expression,
        // NOT macro_invocation. The macro name is the `function` field.
        let src = "SYS_INIT(my_func, POST_KERNEL, 99);\n";
        let (tree, source) = parse_cpp(src);
        let root = tree.root_node();
        let first = (0..root.child_count())
            .filter_map(|i| root.child(i))
            .find(|n| !n.is_error() && n.is_named())
            .expect("at least one named child");
        assert_eq!(first.kind(), "expression_statement");
        let call = first.named_child(0).expect("call_expression child");
        assert_eq!(call.kind(), "call_expression");
        let func = call
            .child_by_field_name("function")
            .expect("function field");
        let name = std::str::from_utf8(&source[func.byte_range()]).unwrap_or("");
        assert_eq!(name, "SYS_INIT");
    }
    #[test]
    fn decl_position_macro_is_also_call_expression() {
        // IMPORTANT: tree-sitter-cpp 0.23.4 parses BOTH statement-position and
        // declaration-position macro calls as expression_statement(call_expression),
        // NOT as macro_invocation.  E.g. `LIST_HEAD(my_list);` at file scope:
        //
        //   (translation_unit
        //     (expression_statement
        //       (call_expression function: (identifier) arguments: (argument_list ...))))
        //
        // This means the "macro_invocation": "macro_call" kind mapping in cpp.json
        // only fires for the narrow cases where tree-sitter genuinely produces a
        // macro_invocation node (very rare in practice with tree-sitter-cpp 0.23.x).
        // Full C/C++ macro-call indexing requires the two-pass pipeline (Task 4.2).
        let src = "LIST_HEAD(my_list);\n";
        let (tree, _source) = parse_cpp(src);
        let root = tree.root_node();
        let first = (0..root.child_count())
            .filter_map(|i| root.child(i))
            .find(|n| !n.is_error() && n.is_named())
            .expect("at least one named child");
        assert_eq!(first.kind(), "expression_statement");
        assert_eq!(
            first.named_child(0).expect("call child").kind(),
            "call_expression"
        );
    }
}
