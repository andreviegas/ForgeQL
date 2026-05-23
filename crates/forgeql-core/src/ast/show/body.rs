//! `SHOW body` implementation.
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{byte_to_line, emit_body_lines, find_function_node_for_symbol};
use crate::{ast::lang::LanguageRegistry, workspace::Workspace};

/// `SHOW body OF 'func' [DEPTH n]`
///
/// Returns the function body as a structured `lines` array with file-absolute
/// 1-based line numbers. `DEPTH 0` returns signature only; `DEPTH n` collapses
/// compound statements deeper than n; absent returns full body.
///
/// # Errors
/// Returns an error if the symbol is not indexed, the file cannot be parsed,
/// or the AST node for the function is not found.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn show_body<S: ::std::hash::BuildHasher>(
    cached: &crate::ast::parse_cache::CachedParse,
    path: &Path,
    byte_range_start: usize,
    // 1-based line number from the index; used to detect misparsed AST nodes.
    hint_line: Option<usize>,
    enrichment: &HashMap<String, String, S>,
    workspace: &Workspace,
    symbol: &str,
    depth: Option<usize>,
    lang_registry: &LanguageRegistry,
) -> Result<Value> {
    let lang = lang_registry
        .language_for_path(path)
        .ok_or_else(|| anyhow!("no language for {}", path.display()))?;
    let config = lang.config();
    let source = &*cached.source;
    let fn_node =
        find_function_node_for_symbol(cached.tree.root_node(), byte_range_start, hint_line, config)
            .ok_or_else(|| anyhow!("function definition for '{symbol}' not found in AST"))?;

    let fn_start = fn_node.start_byte();
    let fn_start_line = byte_to_line(source, fn_start); // 0-based

    let lines: Vec<Value> = match depth {
        None => {
            // Full body — number every line from fn_start_line.
            let text = std::str::from_utf8(&source[fn_node.byte_range()]).unwrap_or("");
            text.split('\n')
                .enumerate()
                .map(|(i, raw)| {
                    serde_json::json!({
                        "line": fn_start_line + i + 1,
                        "text": raw.trim_end_matches('\r'),
                    })
                })
                .collect()
        }
        Some(0) => {
            // Signature only — stop before the body compound_statement.
            let body_start = fn_node
                .child_by_field_name("body")
                .map_or_else(|| fn_node.end_byte(), |b| b.start_byte());
            let sig = std::str::from_utf8(&source[fn_node.start_byte()..body_start])
                .unwrap_or("")
                .trim_end();
            sig.split('\n')
                .enumerate()
                .map(|(i, raw)| {
                    serde_json::json!({
                        "line": fn_start_line + i + 1,
                        "text": raw.trim_end_matches('\r'),
                    })
                })
                .collect()
        }
        Some(n) => emit_body_lines(source, fn_node, n, config)
            .into_iter()
            .map(|(ln, text)| serde_json::json!({ "line": ln, "text": text }))
            .collect(),
    };

    let fn_end_line_raw = fn_node.end_position().row + 1;

    // Re-derive the true end-line boundary by running the absorbed-sibling
    // check live on the already-parsed AST node.  This supersedes the stored
    // `enrichment["lines"]` value, which may be stale from an index built
    // before a fix to the enrichment pipeline (e.g. a local-variable
    // initializer incorrectly counted as an absorbed file-scope declaration).
    // The live check always uses the current, post-fix code path.
    //
    // `absorbed_row` is the 0-based row of the first absorbed child, which
    // mirrors the `end_row` used in the enrichment pipeline:
    //   fn_end_line = absorbed_row + 1  (0-based row → 1-based line)
    let fn_end_line =
        crate::ast::enrich::metrics::first_absorbed_toplevel_in_compound(fn_node, config)
            .map_or(fn_end_line_raw, |absorbed_row| absorbed_row + 1);

    // Clip emitted lines to the true boundary (no-op for clean functions).
    let lines: Vec<Value> = lines
        .into_iter()
        .filter(|l| {
            l["line"]
                .as_u64()
                .is_none_or(|n| usize::try_from(n).is_ok_and(|n| n <= fn_end_line))
        })
        .collect();

    let path_str = workspace.relative(path).display().to_string();

    // When DEPTH 0, include enrichment metadata so the agent can make
    // informed decisions (e.g. how many lines, params, branches) without
    // a separate FIND query.
    let metadata: serde_json::Value = if depth == Some(0) && !enrichment.is_empty() {
        const SELECTED_KEYS: &[&str] = &[
            "lines",
            "param_count",
            "return_count",
            "branch_count",
            "is_recursive",
            "has_todo",
            "has_shadow",
            "has_escape",
            "has_unused_param",
            "enclosing_type",
        ];
        let selected: serde_json::Map<String, serde_json::Value> = enrichment
            .iter()
            .filter(|(k, _)| SELECTED_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        if selected.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(selected)
        }
    } else {
        serde_json::Value::Null
    };

    let mut result = serde_json::json!({
        "op":         "show_body",
        "symbol":     symbol,
        "path":       path_str,
        "start_line": fn_start_line + 1,
        "end_line":   fn_end_line,
        "line":       fn_start_line + 1,
        "byte_start": fn_start,
        "depth":      depth,
        "lines":      lines,
    });
    if !metadata.is_null() {
        result["metadata"] = metadata;
    }
    Ok(result)
}
