//! `SHOW callers`, `SHOW callees`, and `SHOW LINES` implementations.
use std::collections::HashMap;

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{byte_to_line, collect_callees_walk, find_function_node_for_symbol, parse_file};
use crate::{
    ast::{
        index::{IndexRow, SymbolTable},
        lang::LanguageRegistry,
    },
    workspace::Workspace,
};

/// `SHOW callers OF 'func'`
///
/// Returns all reference sites for `symbol` from the usage index, with their
/// file path and 1-based line number.
///
/// # Errors
/// This function currently always returns `Ok`.
pub fn show_callers(index: &SymbolTable, workspace: &Workspace, symbol: &str) -> Result<Value> {
    let sites = index.usages.get(symbol).map_or(&[] as &[_], Vec::as_slice);

    // Cache file bytes keyed by path_id (u32) to avoid re-reading the same
    // file multiple times and to skip any PathBuf allocation per site.
    let mut byte_cache: HashMap<u32, Vec<u8>> = HashMap::new();

    let results: Vec<Value> = sites
        .iter()
        .map(|s| {
            let path = index.strings.paths.get(s.path_id);
            let path_str = workspace.relative(path).display().to_string();
            let src = byte_cache.entry(s.path_id).or_insert_with(|| {
                crate::workspace::file_io::read_bytes(index.strings.paths.get(s.path_id))
                    .unwrap_or_default()
            });
            let line = byte_to_line(src, s.byte_range.start) + 1;
            serde_json::json!({
                "path":       path_str,
                "line":       line,
                "byte_start": s.byte_range.start,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "op":      "show_callers",
        "symbol":  symbol,
        "results": results,
    }))
}

/// `SHOW callees OF 'func'`
///
/// Re-parses the file containing `symbol`, locates the `function_definition`
/// node, then walks its body collecting `call_expression` targets.
///
/// Results are sorted and deduplicated.
///
/// # Errors
/// Returns an error if the file cannot be parsed or the AST node for the
/// function is not found.
pub fn show_callees(
    def: &IndexRow,
    index: &SymbolTable,
    workspace: &Workspace,
    symbol: &str,
    lang_registry: &LanguageRegistry,
) -> Result<Value> {
    let def_path = index.path_of(def);
    let lang = lang_registry
        .language_for_path(def_path)
        .ok_or_else(|| anyhow!("no language for {}", def_path.display()))?;
    let config = lang.config();
    let (source, tree) = parse_file(def_path, lang_registry)?;
    let fn_node = find_function_node_for_symbol(tree.root_node(), def.byte_range.start, config)
        .ok_or_else(|| anyhow!("function definition for '{symbol}' not found in AST"))?;

    let mut callees: Vec<String> = Vec::new();
    collect_callees_walk(
        &source,
        fn_node,
        &mut callees,
        config.call_expression_kind(),
    );
    callees.sort();
    callees.dedup();

    let def_path = index.path_of(def);
    let path_str = workspace.relative(def_path).display().to_string();
    let results: Vec<Value> = callees
        .iter()
        .map(|name| {
            index.find_def(name).map_or_else(
                || serde_json::json!({ "name": name }),
                |callee_def| {
                    let callee_path = workspace
                        .relative(index.path_of(callee_def))
                        .display()
                        .to_string();
                    serde_json::json!({
                        "name": name,
                        "path": callee_path,
                        "line": callee_def.line,
                    })
                },
            )
        })
        .collect();

    Ok(serde_json::json!({
        "op":      "show_callees",
        "symbol":  symbol,
        "path":    path_str,
        "results": results,
    }))
}

// -----------------------------------------------------------------------
// Raw file views — no tree-sitter needed
// -----------------------------------------------------------------------

/// `SHOW LINES n-m OF 'file'` — return 1-based inclusive line range.
///
/// Both `start_line` and `end_line` are 1-based.  `end_line` is clamped to
/// the file's last line if it exceeds the actual line count.  The response
/// includes an annotation per line and a `byte_start` field marking the byte
/// offset of the first line so callers can relate the result to byte-range
/// SHOW AT queries.
///
/// # Errors
/// Returns `Err` when the file cannot be read, the file is not valid UTF-8,
/// or `start_line` is 0 or beyond the last line.
pub fn show_lines(
    workspace: &Workspace,
    file: &str,
    start_line: usize,
    end_line: usize,
) -> Result<Value> {
    let path = workspace.safe_path(file)?;
    let source = crate::workspace::file_io::read_bytes(&path)?;
    let text = std::str::from_utf8(&source)
        .map_err(|e| anyhow!("file '{file}' is not valid UTF-8: {e}"))?;

    let all_lines: Vec<&str> = text.split('\n').collect();
    let total = all_lines.len();

    if start_line == 0 || start_line > total {
        return Err(anyhow!(
            "start_line {start_line} is out of range (file has {total} line(s))"
        ));
    }
    let clamped_end = end_line.min(total);

    if start_line > clamped_end {
        return Err(anyhow!(
            "start_line {start_line} is greater than end_line {clamped_end}"
        ));
    }

    // Compute the byte offset of the first requested line.
    let byte_start: usize = all_lines[..start_line - 1]
        .iter()
        .map(|l| l.len() + 1) // +1 for the '\n'
        .sum();

    let lines: Vec<Value> = all_lines[start_line - 1..clamped_end]
        .iter()
        .enumerate()
        .map(|(i, line_text)| {
            serde_json::json!({
                "line": start_line + i,
                "text": line_text,
            })
        })
        .collect();

    Ok(serde_json::json!({
        "op":         "show_lines",
        "file":       file,
        "start_line": start_line,
        "end_line":   clamped_end,
        "byte_start": byte_start,
        "lines":      lines,
    }))
}
