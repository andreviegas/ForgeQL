//! Macro-expansion enricher — adds metadata fields to `macro_call` rows.
//!
//! When the second-pass [`EnrichContext`] carries a populated
//! [`macro_table::MacroTable`], this enricher looks up the macro name,
//! performs a single-level substitution (no recursive expansion), and
//! records the result as extra fields on the row.
//!
//! Fields added to `macro_call` rows:
//! - `macro_def_file`   — source file of the selected definition
//! - `macro_def_line`   — 1-based line of the selected definition
//! - `macro_arity`      — number of parameters (`"0"` for object-like macros)
//! - `macro_expansion`  — expanded body text with arguments substituted
//!   (empty string when the macro table is unavailable
//!   or the name is not found)

use std::collections::HashMap;

use super::{EnrichContext, NodeEnricher};
use crate::ast::enrich::macro_resolve::{ExpansionBudget, resolve_macro};

/// Enricher that resolves macro invocations against the [`MacroTable`].
pub struct MacroExpandEnricher;

impl NodeEnricher for MacroExpandEnricher {
    fn name(&self) -> &'static str {
        "macro_expand"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        // Determine if this is a macro_call row.  Two paths:
        // 1. Rust: tree-sitter produces macro_invocation → map_kind = "macro_call"
        // 2. C++:  tree-sitter produces call_expression → name is in MacroTable
        let is_macro_call =
            ctx.language_support.map_kind(ctx.node.kind()).unwrap_or("") == "macro_call" || {
                let call_kind = ctx.language_config.call_expression_kind();
                !call_kind.is_empty()
                    && ctx.node.kind() == call_kind
                    && ctx.macro_table.is_some_and(|t| t.contains(name))
            };
        if !is_macro_call {
            return;
        }

        let Some(table) = ctx.macro_table else { return };
        let Some(expander) = ctx.language_support.macro_expander() else {
            return;
        };

        // Extract call arguments from the invocation node.
        let args = expander.extract_args(ctx.node, ctx.source);

        // Attempt a single-level resolution (budget: 1 step, depth 1).
        let mut budget = ExpansionBudget {
            max_depth: 1,
            max_steps: 1,
            steps_remaining: 1,
        };

        if let Some(result) = resolve_macro(table, name, &args, expander, &mut budget, 0) {
            let def = &result.resolved_def;

            drop(fields.insert(
                "macro_def_file".to_owned(),
                def.file.to_string_lossy().into_owned(),
            ));
            drop(fields.insert("macro_def_line".to_owned(), def.line.to_string()));
            drop(fields.insert(
                "macro_arity".to_owned(),
                def.params.as_ref().map_or(0, Vec::len).to_string(),
            ));
            drop(fields.insert("macro_expansion".to_owned(), result.expanded.clone()));
            drop(fields.insert("expansion_depth".to_owned(), result.depth.to_string()));

            // Extract identifiers from the expanded text as reads.
            let reads: Vec<&str> = result
                .expanded
                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .filter(|w| w.len() > 1 && w.as_bytes()[0].is_ascii_alphabetic())
                .collect();
            if !reads.is_empty() {
                drop(fields.insert("expanded_reads".to_owned(), reads.join(",")));
            }

            // Check for address-of in expansion.
            if result.expanded.contains('&') {
                drop(fields.insert("expanded_has_escape".to_owned(), "true".to_owned()));
            }
        } else {
            drop(fields.insert("expansion_failed".to_owned(), "true".to_owned()));
            let reason = if !table.contains(name) {
                "no_definition"
            } else if budget.steps_remaining == 0 {
                "budget_exhausted"
            } else {
                "unknown"
            };
            drop(fields.insert("expansion_failure_reason".to_owned(), reason.to_owned()));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enricher_name() {
        assert_eq!(MacroExpandEnricher.name(), "macro_expand");
    }
}
