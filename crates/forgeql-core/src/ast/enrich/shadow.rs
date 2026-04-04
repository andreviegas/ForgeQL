/// Shadow enrichment — detects functions where an inner scope redeclares a
/// variable name that already exists in an outer scope.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `has_shadow`:   `"true"` if any shadowed variable was detected.
/// - `shadow_count`: number of distinct shadowed variable names.
/// - `shadow_vars`:  comma-separated names of shadowed variables.
///
/// A "shadow" occurs when a declaration in a nested scope uses the same
/// identifier as a declaration in an enclosing scope of the same function.
/// Parameters are treated as the outermost scope.
///
/// **Language-agnostic via config:** `scope_creating_raw_kinds` controls which
/// node kinds open a new scope.  C++/Rust: `["compound_statement"]` / `["block"]`
/// (every `{}` creates a new scope).  Python: only `function_definition`,
/// `class_definition`, `lambda`, and comprehension nodes create scopes — `if`/
/// `for` blocks do NOT, which matches Python's function-scoped variable rules.
use std::collections::{BTreeSet, HashMap};

use super::data_flow_utils::{collect_parameter_names, extract_declarator_name};
use super::{EnrichContext, NodeEnricher};
use crate::ast::lang::LanguageConfig;

pub struct ShadowEnricher;

impl NodeEnricher for ShadowEnricher {
    fn name(&self) -> &'static str {
        "shadow"
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

        let source = ctx.source;
        let func = ctx.node;

        let params: BTreeSet<String> = collect_parameter_names(func, source, config)
            .into_iter()
            .collect();

        let Some(body) = func.child_by_field_name("body") else {
            return;
        };

        let mut shadowed: BTreeSet<String> = BTreeSet::new();
        walk_scopes_iterative(body, source, config, params, &mut shadowed);

        if !shadowed.is_empty() {
            drop(fields.insert("has_shadow".into(), "true".into()));
            drop(fields.insert("shadow_count".into(), shadowed.len().to_string()));
            let vars: Vec<&str> = shadowed.iter().map(String::as_str).collect();
            drop(fields.insert("shadow_vars".into(), vars.join(",")));
        }
    }
}

/// Work item for the iterative scope-aware tree walk.
enum WorkItem<'tree> {
    /// Visit this node.
    Visit {
        node: tree_sitter::Node<'tree>,
        /// `true` when this node is a direct child of a scope-creating block.
        /// Direct children check only outer scopes for shadowing; non-direct
        /// children (e.g. for-loop initializers) also check the current scope.
        in_block_direct: bool,
    },
    /// Restore scope state after finishing a scope-creating block.
    ExitScope { saved_current: BTreeSet<String> },
}

/// Iterative (non-recursive) scope-aware shadow walk.
///
/// Uses an explicit work stack to avoid call-stack depth issues on deeply
/// nested code.  `scope_stack` holds outer scopes (innermost-last).
/// `current_scope` accumulates declarations at the current nesting level.
fn walk_scopes_iterative(
    body: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    params: BTreeSet<String>,
    shadowed: &mut BTreeSet<String>,
) {
    // In Python-style languages the params and function body share one scope:
    // start with params already in `current_scope` and an empty outer stack.
    // In C++/Rust-style languages params are an outer scope and the function
    // body is an inner scope.
    let (mut scope_stack, mut current_scope) = if config.params_share_body_scope() {
        (Vec::<BTreeSet<String>>::new(), params)
    } else {
        (vec![params], BTreeSet::new())
    };

    // Seed with body's direct children in reverse so they pop in forward order.
    let mut work: Vec<WorkItem<'_>> = Vec::new();
    for i in (0..body.child_count()).rev() {
        if let Some(child) = body.child(i) {
            work.push(WorkItem::Visit {
                node: child,
                in_block_direct: true,
            });
        }
    }

    while let Some(item) = work.pop() {
        match item {
            WorkItem::ExitScope { saved_current } => {
                let _ = scope_stack.pop();
                current_scope = saved_current;
            }
            WorkItem::Visit {
                node,
                in_block_direct,
            } => {
                let kind = node.kind();

                if config.is_scope_creating_kind(kind) {
                    // Open a new scope: save current state, push it for inner
                    // block to check against, start fresh current_scope.
                    let saved = std::mem::take(&mut current_scope);
                    scope_stack.push(saved.clone());
                    work.push(WorkItem::ExitScope {
                        saved_current: saved,
                    });
                    for i in (0..node.child_count()).rev() {
                        if let Some(child) = node.child(i) {
                            work.push(WorkItem::Visit {
                                node: child,
                                in_block_direct: true,
                            });
                        }
                    }
                } else if config.is_declaration_kind(kind) {
                    if let Some(name) = extract_declarator_name(node, source, config) {
                        // Direct children of a block: shadow only if name is in an outer scope.
                        // Non-direct (for-loop init etc.): also shadow if in current scope.
                        // For Python-style languages, only outer scopes (scope_stack)
                        // trigger shadows. The `current_scope` check is suppressed
                        // because params live there and reassigning a param is not a shadow.
                        let is_shadow = scope_stack.iter().any(|s| s.contains(&name))
                            || (!config.params_share_body_scope()
                                && !in_block_direct
                                && current_scope.contains(&name));
                        if is_shadow {
                            let _ = shadowed.insert(name.clone());
                        }
                        let _ = current_scope.insert(name);
                    }
                } else {
                    // Non-scope, non-declaration: recurse into children (non-direct context).
                    for i in (0..node.child_count()).rev() {
                        if let Some(child) = node.child(i) {
                            work.push(WorkItem::Visit {
                                node: child,
                                in_block_direct: false,
                            });
                        }
                    }
                }
            }
        }
    }
}
