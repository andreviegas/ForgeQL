/// Unused-parameter enrichment — detects function parameters that are
/// never referenced in the function body.
///
/// `enrich_row()` adds to `function_definition` rows:
/// - `has_unused_param`:  `"true"` if any parameter is unreferenced.
/// - `unused_param_count`: number of unused parameters.
/// - `unused_params`:      comma-separated names of unused parameters.
///
/// **Language-agnostic:** uses `function_raw_kinds`, `parameter_list_raw_kind`,
/// `parameter_raw_kind`, `declarator_field_name`, `identifier_raw_kind` from
/// [`LanguageConfig`].
use std::collections::{BTreeSet, HashMap};

use super::data_flow_utils::find_leaf_identifier;
use super::{EnrichContext, NodeEnricher};
use crate::ast::index::node_text;
use crate::ast::lang::LanguageConfig;

/// Enricher for unused parameter detection.
pub struct UnusedParamEnricher;

impl NodeEnricher for UnusedParamEnricher {
    fn name(&self) -> &'static str {
        "unused_param"
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

        // Collect parameter names.
        let params = collect_param_names(func, source, config);
        if params.is_empty() {
            return;
        }

        // Get the function body.
        let Some(body) = func.child_by_field_name("body") else {
            return;
        };

        // Collect all identifiers referenced in the body.
        let mut referenced = BTreeSet::new();
        collect_identifiers(body, source, config, &mut referenced);

        // Find parameters not referenced in the body.
        let unused: BTreeSet<&str> = params
            .iter()
            .filter(|p| !referenced.contains(p.as_str()))
            .map(String::as_str)
            .collect();

        if !unused.is_empty() {
            drop(fields.insert("has_unused_param".into(), "true".into()));
            drop(fields.insert("unused_param_count".into(), unused.len().to_string()));
            let names: Vec<&str> = unused.into_iter().collect();
            drop(fields.insert("unused_params".into(), names.join(",")));
        }
    }
}

/// Collect all parameter names from a function node.
fn collect_param_names(
    func: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Vec<String> {
    let mut names = Vec::new();

    let Some(param_list) = find_descendant_by_kind(func, config.parameter_list_raw_kind) else {
        return names;
    };

    for i in 0..param_list.child_count() {
        if let Some(child) = param_list.child(i)
            && config.is_parameter_kind(child.kind())
            && let Some(decl) = child.child_by_field_name(config.declarator_field())
            && let Some(name) = find_leaf_identifier(decl, source, config)
        {
            names.push(name);
        }
    }
    names
}

/// Recursively collect all identifier texts inside a subtree.
fn collect_identifiers(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
    out: &mut BTreeSet<String>,
) {
    let mut cursor = node.walk();
    let mut visit = true;
    loop {
        if visit && config.is_identifier_kind(cursor.node().kind()) {
            let text = node_text(source, cursor.node());
            if !text.is_empty() {
                let _ = out.insert(text);
            }
        }

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
                return;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}

/// Find a descendant node of the given kind (DFS, stops at first match).
fn find_descendant_by_kind<'a>(
    root: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    let mut cursor = root.walk();
    let mut visit = true;
    loop {
        if visit && cursor.node().kind() == kind && cursor.node() != root {
            return Some(cursor.node());
        }
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
                return None;
            }
            if cursor.goto_next_sibling() {
                visit = true;
                break;
            }
        }
    }
}
