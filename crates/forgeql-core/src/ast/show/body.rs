//! `SHOW body` implementation.
use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{byte_to_line, emit_body_lines, find_function_node_for_symbol, parse_file};
use crate::{
    ast::{
        index::{IndexRow, SymbolTable},
        lang::LanguageRegistry,
    },
    workspace::Workspace,
};

/// `SHOW body OF 'func' [DEPTH n]`
///
/// Returns the function body as a structured `lines` array with file-absolute
/// 1-based line numbers. `DEPTH 0` returns signature only; `DEPTH n` collapses
/// compound statements deeper than n; absent returns full body.
///
/// # Errors
/// Returns an error if the symbol is not indexed, the file cannot be parsed,
/// or the AST node for the function is not found.
pub fn show_body(
    def: &IndexRow,
    table: &SymbolTable,
    workspace: &Workspace,
    symbol: &str,
    depth: Option<usize>,
    lang_registry: &LanguageRegistry,
) -> Result<Value> {
    let def_path = table.path_of(def);
    let lang = lang_registry
        .language_for_path(def_path)
        .ok_or_else(|| anyhow!("no language for {}", def_path.display()))?;
    let config = lang.config();
    let (source, tree) = parse_file(def_path, lang_registry)?;
    let fn_node = find_function_node_for_symbol(tree.root_node(), def.byte_range.start, config)
        .ok_or_else(|| anyhow!("function definition for '{symbol}' not found in AST"))?;

    let fn_start = fn_node.start_byte();
    let fn_start_line = byte_to_line(&source, fn_start); // 0-based

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
        Some(n) => emit_body_lines(&source, fn_node, n, config)
            .into_iter()
            .map(|(ln, text)| serde_json::json!({ "line": ln, "text": text }))
            .collect(),
    };

    let fn_end_line = fn_node.end_position().row + 1;
    let path_str = workspace.relative(table.path_of(def)).display().to_string();

    // When DEPTH 0, include enrichment metadata so the agent can make
    // informed decisions (e.g. how many lines, params, branches) without
    // a separate FIND query.
    let metadata: serde_json::Value = if depth == Some(0) && !def.fields.is_empty() {
        let selected: serde_json::Map<String, serde_json::Value> = def
            .fields
            .iter()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "lines"
                        | "param_count"
                        | "return_count"
                        | "branch_count"
                        | "is_recursive"
                        | "has_todo"
                        | "has_shadow"
                        | "has_escape"
                        | "has_unused_param"
                        | "enclosing_type"
                )
            })
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
