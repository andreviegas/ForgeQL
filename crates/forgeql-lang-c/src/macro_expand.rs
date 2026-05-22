//! C macro expander for the two-pass macro-expansion pipeline.
//!
//! [`CMacroExpander`] implements [`MacroExpander`] for the C tree-sitter
//! grammar.  It handles both object-like (`#define FOO 42`) and function-like
//! (`#define MAX(a,b) ((a)>(b)?(a):(b))`) macros.

use std::borrow::Cow;

use forgeql_core::ast::lang::{LanguageConfig, MacroDef, MacroExpander, node_text};

// -----------------------------------------------------------------------
// CMacroExpander
/// [`MacroExpander`] implementation for C.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct CMacroExpander;

impl MacroExpander for CMacroExpander {
    /// Extract a macro definition from a `preproc_def` or
    /// `preproc_function_def` AST node.
    ///
    /// Returns `None` when `node` is not a C macro definition kind.
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
