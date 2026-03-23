/// Declaration distance enrichment — measures how far local variables are
/// declared from their first use.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `decl_distance`: sum of (`first_use_line` − `declaration_line`) for all
///   local variables whose distance ≥ 2.  Higher values indicate more
///   "scattered" declarations.
/// - `decl_far_count`: number of local variables with distance ≥ 2.
/// - `has_unused_reassign`: `"true"` if any local variable is reassigned
///   before its previous value was read (dead store).
///
/// **Exclusions:** parameters, globals, member accesses (`this->x`, `m_x`).
/// **Inclusions:** all locals including loop variables, const locals, nested
/// scope locals.
use std::collections::HashMap;

use super::data_flow_utils::{
    collect_local_declarations, is_compound_assign_or_update, is_in_declaration, is_write_context,
    LocalDecl,
};
use super::{EnrichContext, NodeEnricher};

/// Minimum distance threshold — locals closer than this are considered fine.
const FAR_THRESHOLD: usize = 2;

/// Enricher for variable declaration distance and dead-store detection.
pub struct DeclDistanceEnricher;

impl NodeEnricher for DeclDistanceEnricher {
    fn name(&self) -> &'static str {
        "decl_distance"
    }

    fn enrich_row(
        &self,
        ctx: &EnrichContext<'_>,
        _name: &str,
        fields: &mut HashMap<String, String>,
    ) {
        let config = ctx.language_config;
        if !config.function_raw_kinds.contains(&ctx.node.kind()) {
            return;
        }

        // 1. Collect local variable declarations (name → declaration line).
        let locals = collect_local_declarations(ctx);
        if locals.is_empty() {
            drop(fields.insert("decl_distance".to_string(), "0".to_string()));
            drop(fields.insert("decl_far_count".to_string(), "0".to_string()));
            drop(fields.insert("has_unused_reassign".to_string(), "false".to_string()));
            return;
        }

        // 2. Walk identifiers to find first use of each local & detect dead stores.
        let (first_uses, has_dead_store) = analyse_uses(ctx, &locals);

        // 3. Compute distance metrics.
        let mut total_distance: usize = 0;
        let mut far_count: usize = 0;

        for (name, decl_line) in &locals {
            if let Some(&first_use_line) = first_uses.get(name.as_str()) {
                let dist = first_use_line.saturating_sub(*decl_line);
                if dist >= FAR_THRESHOLD {
                    total_distance += dist;
                    far_count += 1;
                }
            }
            // If no use found, the variable is unused — decl_distance doesn't
            // count it (other linters catch unused variables).
        }

        drop(fields.insert("decl_distance".to_string(), total_distance.to_string()));
        drop(fields.insert("decl_far_count".to_string(), far_count.to_string()));
        drop(fields.insert(
            "has_unused_reassign".to_string(),
            has_dead_store.to_string(),
        ));
    }
}

/// Analyse identifier uses within a function body.
///
/// Returns:
/// - `first_uses`: map of local name → first-use 1-based line
/// - `has_dead_store`: whether any local was written twice without a read
///   in between (excludes compound assignments like `+=`)
fn analyse_uses<'a>(
    ctx: &EnrichContext<'a>,
    locals: &[LocalDecl],
) -> (HashMap<&'a str, usize>, bool) {
    let source = ctx.source;
    let func = ctx.node;
    let config = ctx.language_config;

    // Build a set of local names for quick lookup.
    // We store the name as owned strings and use a HashSet for O(1) membership.
    let local_names: HashMap<&str, usize> = locals
        .iter()
        .map(|(name, line)| (name.as_str(), *line))
        .collect();

    // first_uses: name → first-use line (1-based)
    let mut first_uses: HashMap<&str, usize> = HashMap::new();

    // Dead-store tracking: name → state (true = written-not-yet-read)
    let mut written_not_read: HashMap<&str, bool> = HashMap::new();
    let mut has_dead_store = false;

    // Seed: declarations with initializers (int x = expr;) count as an
    // initial write.  Uninitialized declarations (int x;) don't.
    for (name, _) in locals {
        // Check if the declaration has a "value" child in the init_declarator,
        // indicating it's initialized.  We conservatively assume initialized
        // (most C++ locals have initializers).
        let _ = written_not_read.insert(name.as_str(), true);
    }

    // Walk all nodes in source order.
    let mut cursor = func.walk();
    let mut visit = true;

    loop {
        if visit {
            let node = cursor.node();
            let kind = node.kind();

            // Only interested in identifier nodes that could be local variable refs.
            if kind == config.identifier_raw_kind && node != func {
                let text = std::str::from_utf8(&source[node.byte_range()]).unwrap_or("");

                if let Some(&decl_line) = local_names.get(text) {
                    let use_line = node.start_position().row + 1;

                    // Skip the identifier that IS the declaration itself.
                    if use_line != decl_line || !is_in_declaration(node, config) {
                        // Track first use.
                        let _ = first_uses.entry(text).or_insert(use_line);

                        // Dead-store tracking.
                        let is_write = is_write_context(node, config);
                        let is_compound = is_compound_assign_or_update(node, config);

                        if is_compound {
                            // Compound assign (+=, etc.) or ++/-- reads AND writes.
                            // This counts as a read, so clear the written flag.
                            let _ = written_not_read.insert(text, true);
                        } else if is_write {
                            // Pure write (simple =).
                            if written_not_read.get(text) == Some(&true) {
                                has_dead_store = true;
                            }
                            let _ = written_not_read.insert(text, true);
                        } else {
                            // Read.
                            let _ = written_not_read.insert(text, false);
                        }
                    }
                }
            }
        }

        // DFS traversal
        if visit && cursor.goto_first_child() {
            visit = true;
            continue;
        }
        if cursor.goto_next_sibling() {
            visit = true;
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return (first_uses, has_dead_store);
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}
