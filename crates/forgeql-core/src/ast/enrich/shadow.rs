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
use super::guard_utils::{
    GuardFrame, GuardInfo, are_guards_exclusive, guard_info_from_stack, update_guard_stack,
};
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
    ExitScope {
        saved_current: HashMap<String, Option<GuardInfo>>,
    },
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
    // Convert params to guard-aware map.  Params are always unconditional.
    let params_map: HashMap<String, Option<GuardInfo>> =
        params.into_iter().map(|p| (p, None)).collect();

    // In Python-style languages the params and function body share one scope:
    // start with params already in `current_scope` and an empty outer stack.
    // In C++/Rust-style languages params are an outer scope and the function
    // body is an inner scope.
    let (scope_stack, current_scope) = if config.params_share_body_scope() {
        (Vec::new(), params_map)
    } else {
        (vec![params_map], HashMap::new())
    };
    let mut tracker = ScopeTracker {
        scope_stack,
        current_scope,
    };

    // Mini guard stack: maintained in parallel with the tree walk using the
    // same byte-range based push/pop logic as `collect_nodes()`.
    let mut mini_guard_stack: Vec<GuardFrame> = Vec::new();

    // Seed with body's direct children in reverse so they pop in forward order.
    let mut work: Vec<WorkItem<'_>> = Vec::new();
    push_children(&mut work, body, true);

    while let Some(item) = work.pop() {
        match item {
            WorkItem::ExitScope { saved_current } => tracker.exit_scope(saved_current),
            WorkItem::Visit {
                node,
                in_block_direct,
            } => {
                let kind = node.kind();
                update_guard_stack(node, source, config, &mut mini_guard_stack);

                if config.is_scope_creating_kind(kind) {
                    tracker.open_scope(&mut work, node);
                } else if config.is_declaration_kind(kind) {
                    tracker.record_declaration(
                        node,
                        source,
                        config,
                        in_block_direct,
                        &mini_guard_stack,
                        shadowed,
                    );
                } else {
                    // Non-scope, non-declaration: recurse into children.
                    push_children(&mut work, node, false);
                }
            }
        }
    }
}

/// Live scope chain for the iterative shadow-detection walk: `scope_stack` holds
/// the enclosing scopes (outermost first) and `current_scope` the innermost one.
struct ScopeTracker {
    scope_stack: Vec<HashMap<String, Option<GuardInfo>>>,
    current_scope: HashMap<String, Option<GuardInfo>>,
}

impl ScopeTracker {
    /// Leave the current scope, restoring the one saved when it was opened.
    fn exit_scope(&mut self, saved_current: HashMap<String, Option<GuardInfo>>) {
        let _ = self.scope_stack.pop();
        self.current_scope = saved_current;
    }

    /// Open a new scope: stash the current one (on the chain for inner lookups
    /// and on the work stack for restoration), then queue the node's children.
    fn open_scope<'a>(&mut self, work: &mut Vec<WorkItem<'a>>, node: tree_sitter::Node<'a>) {
        let saved = std::mem::take(&mut self.current_scope);
        self.scope_stack.push(saved.clone());
        work.push(WorkItem::ExitScope {
            saved_current: saved,
        });
        push_children(work, node, true);
    }

    /// Record a declaration: flag a shadow when the name already exists in an
    /// outer scope (or, for non-direct declarations, the current scope) under a
    /// non-exclusive guard, then bind the name in the current scope.
    fn record_declaration(
        &mut self,
        node: tree_sitter::Node<'_>,
        source: &[u8],
        config: &LanguageConfig,
        in_block_direct: bool,
        guard_stack: &[GuardFrame],
        shadowed: &mut BTreeSet<String>,
    ) {
        let Some(name) = extract_declarator_name(node, source, config) else {
            return;
        };
        let decl_guard = guard_info_from_stack(guard_stack);

        // Direct children of a block: shadow only if name is in an outer scope.
        // Non-direct (for-loop init etc.): also shadow if in current scope.
        // Guard exclusivity suppresses false positives from #ifdef/#else siblings.
        let is_outer_shadow = self.scope_stack.iter().any(|s| {
            s.get(&name).is_some_and(|existing| {
                !guards_exclusive_opt(existing.as_ref(), decl_guard.as_ref())
            })
        });
        let is_current_shadow = !config.params_share_body_scope()
            && !in_block_direct
            && self.current_scope.get(&name).is_some_and(|existing| {
                !guards_exclusive_opt(existing.as_ref(), decl_guard.as_ref())
            });

        if is_outer_shadow || is_current_shadow {
            let _ = shadowed.insert(name.clone());
        }
        let _ = self.current_scope.insert(name, decl_guard);
    }
}

/// Queue a node's children for visiting, in reverse so they pop in source order.
fn push_children<'a>(
    work: &mut Vec<WorkItem<'a>>,
    node: tree_sitter::Node<'a>,
    in_block_direct: bool,
) {
    for i in (0..node.child_count()).rev() {
        if let Some(child) = node.child(i) {
            work.push(WorkItem::Visit {
                node: child,
                in_block_direct,
            });
        }
    }
}

/// Returns `true` if `a` and `b` are in structurally exclusive guard branches.
/// Returns `false` if either is unconditional (no guard).
#[inline]
fn guards_exclusive_opt(a: Option<&GuardInfo>, b: Option<&GuardInfo>) -> bool {
    match (a, b) {
        (Some(ga), Some(gb)) => are_guards_exclusive(ga, gb),
        _ => false,
    }
}
