//! Macro expansion resolution for the two-pass pipeline.
//!
//! [`resolve_macro`] looks up a macro invocation in a [`MacroTable`],
//! selects the best matching definition (respecting guard context), and
//! performs parameter substitution via a language-supplied [`MacroExpander`].

use crate::ast::enrich::macro_table::{MacroDef, MacroTable};
use crate::ast::lang::MacroExpander;

// -----------------------------------------------------------------------
// ExpansionBudget
// -----------------------------------------------------------------------

/// Controls the depth and step limits of recursive macro expansion.
///
/// Pass a `ExpansionBudget` to [`resolve_macro`] to prevent runaway expansion
/// (e.g. `A → B → ... → A`) from catastrophically spending compute.
#[derive(Debug, Clone)]
pub struct ExpansionBudget {
    /// Maximum nesting depth for recursive macro expansion.
    pub max_depth: usize,
    /// Maximum total substitution steps across the entire expansion tree.
    pub max_steps: usize,
    /// Remaining step budget (decremented by each substitution).
    pub steps_remaining: usize,
}

impl ExpansionBudget {
    /// Create a budget with sensible defaults (depth 8, 64 steps).
    #[must_use]
    pub const fn default_budget() -> Self {
        Self {
            max_depth: 8,
            max_steps: 64,
            steps_remaining: 64,
        }
    }

    /// Whether any expansion steps remain.
    #[must_use]
    pub const fn has_steps(&self) -> bool {
        self.steps_remaining > 0
    }

    /// Consume one step.  Returns `true` when the step was consumed
    /// successfully, `false` when the budget was already exhausted.
    pub const fn consume(&mut self) -> bool {
        if self.steps_remaining > 0 {
            self.steps_remaining -= 1;
            true
        } else {
            false
        }
    }
}

// -----------------------------------------------------------------------
// MacroResolveResult
// -----------------------------------------------------------------------

/// The result of a successful macro expansion.
#[derive(Debug, Clone)]
pub struct MacroResolveResult {
    /// The fully-substituted expansion text.
    pub expanded: String,
    /// The definition that was selected for expansion.
    pub resolved_def: MacroDef,
    /// Nesting depth at which this expansion occurred.
    pub depth: usize,
}

// -----------------------------------------------------------------------
// resolve_macro
// -----------------------------------------------------------------------

/// Attempt to expand a macro invocation.
///
/// * `table`   — the macro definition table built during the first pass.
/// * `name`    — the macro name at the call site.
/// * `args`    — argument texts extracted from the invocation node.
/// * `expander`— the language-specific [`MacroExpander`].
/// * `budget`  — expansion depth/step limits (consumed in-place).
/// * `depth`   — current nesting depth (pass `0` at the call site).
///
/// Returns `None` when:
/// - the macro name is not in `table`,
/// - the definition is an object-like macro called with arguments (arity
///   mismatch), or
/// - the budget is exhausted.
#[must_use]
pub fn resolve_macro(
    table: &MacroTable,
    name: &str,
    args: &[String],
    expander: &dyn MacroExpander,
    budget: &mut ExpansionBudget,
    depth: usize,
) -> Option<MacroResolveResult> {
    if depth >= budget.max_depth || !budget.consume() {
        return None;
    }

    let defs = table.get(name);
    if defs.is_empty() {
        return None;
    }

    // Select the best matching definition.
    // Preference: matching arity first, then any object-like def.
    let def = select_def(defs, args.len())?;

    let params: &[String] = def.params.as_deref().unwrap_or(&[]);
    let substituted = expander.substitute(&def.body, params, args);

    Some(MacroResolveResult {
        expanded: substituted,
        resolved_def: def.clone(),
        depth,
    })
}

/// Select the best macro definition from a slice for the given argument count.
///
/// Returns `None` when no definition can accept the given number of arguments.
fn select_def(defs: &[MacroDef], arg_count: usize) -> Option<&MacroDef> {
    // Prefer a function-like def whose arity matches exactly.
    if let Some(def) = defs
        .iter()
        .find(|d| d.params.as_ref().is_some_and(|p| p.len() == arg_count))
    {
        return Some(def);
    }
    // Fall back to an object-like def only when no args were provided.
    if arg_count == 0
        && let Some(def) = defs.iter().find(|d| d.params.is_none())
    {
        return Some(def);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::enrich::macro_table::MacroTable;
    use std::borrow::Cow;
    use std::path::PathBuf;

    struct SimpleExpander;

    impl MacroExpander for SimpleExpander {
        fn extract_def(
            &self,
            _node: tree_sitter::Node<'_>,
            _source: &[u8],
            _config: &crate::ast::lang::LanguageConfig,
        ) -> Option<MacroDef> {
            None
        }

        fn extract_args(&self, _node: tree_sitter::Node<'_>, _source: &[u8]) -> Vec<String> {
            Vec::new()
        }

        fn substitute(&self, body: &str, params: &[String], args: &[String]) -> String {
            let mut result = body.to_owned();
            for (param, arg) in params.iter().zip(args.iter()) {
                result = result.replace(param.as_str(), arg.as_str());
            }
            result
        }

        fn wrap_for_reparse<'a>(&self, expanded: &'a str) -> Cow<'a, str> {
            Cow::Borrowed(expanded)
        }
    }

    fn make_fn_def(name: &str, params: &[&str], body: &str) -> MacroDef {
        MacroDef {
            name: name.to_owned(),
            params: Some(params.iter().map(|s| (*s).to_owned()).collect()),
            body: body.to_owned(),
            file: PathBuf::from("test.cpp"),
            line: 1,
            guard_group_id: None,
            guard_branch: None,
        }
    }

    fn make_obj_def(name: &str, body: &str) -> MacroDef {
        MacroDef {
            name: name.to_owned(),
            params: None,
            body: body.to_owned(),
            file: PathBuf::from("test.cpp"),
            line: 1,
            guard_group_id: None,
            guard_branch: None,
        }
    }

    #[test]
    fn expand_function_like() {
        let mut table = MacroTable::new();
        table.insert(make_fn_def("MAX", &["a", "b"], "(a) > (b) ? (a) : (b)"));
        let expander = SimpleExpander;
        let mut budget = ExpansionBudget::default_budget();
        let result = resolve_macro(
            &table,
            "MAX",
            &["x".to_owned(), "y".to_owned()],
            &expander,
            &mut budget,
            0,
        );
        let result = result.expect("should expand");
        assert_eq!(result.expanded, "(x) > (y) ? (x) : (y)");
    }

    #[test]
    fn expand_object_like() {
        let mut table = MacroTable::new();
        table.insert(make_obj_def("PI", "3.14159f"));
        let expander = SimpleExpander;
        let mut budget = ExpansionBudget::default_budget();
        let result = resolve_macro(&table, "PI", &[], &expander, &mut budget, 0);
        let result = result.expect("should expand");
        assert_eq!(result.expanded, "3.14159f");
    }

    #[test]
    fn unknown_macro_returns_none() {
        let table = MacroTable::new();
        let expander = SimpleExpander;
        let mut budget = ExpansionBudget::default_budget();
        let result = resolve_macro(&table, "UNKNOWN", &[], &expander, &mut budget, 0);
        assert!(result.is_none());
    }

    #[test]
    fn budget_exhausted_returns_none() {
        let mut table = MacroTable::new();
        table.insert(make_obj_def("A", "1"));
        let expander = SimpleExpander;
        let mut budget = ExpansionBudget {
            max_depth: 8,
            max_steps: 0,
            steps_remaining: 0,
        };
        let result = resolve_macro(&table, "A", &[], &expander, &mut budget, 0);
        assert!(result.is_none());
    }
}
