//! `SHOW outline` and `SHOW members` implementations.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{byte_to_line, find_type_node_by_name, parse_file, path_matches};
use crate::{
    ast::{
        index::{IndexRow, SymbolTable},
        lang::LanguageRegistry,
    },
    workspace::Workspace,
};

/// `SHOW outline OF 'file'`
///
/// Returns all indexed symbols in the file, sorted by byte offset, each with
/// its kind and 1-based line number. The `file` argument may be a glob pattern.
///
/// # Errors
/// Returns an error only if the workspace cannot be accessed
/// (in practice, always returns `Ok`).
pub fn show_outline(index: &SymbolTable, workspace: &Workspace, file: &str) -> Result<Value> {
    let root = workspace.root();

    // Cache file bytes to avoid re-reading the same file for every symbol.
    let mut byte_cache: HashMap<PathBuf, Vec<u8>> = HashMap::new();

    let line_for = |cache: &mut HashMap<PathBuf, Vec<u8>>, path: &Path, offset: usize| -> usize {
        let src = cache
            .entry(path.to_path_buf())
            .or_insert_with(|| crate::workspace::file_io::read_bytes(path).unwrap_or_default());
        byte_to_line(src, offset) + 1
    };

    let mut entries: Vec<(usize, Value)> = Vec::new();

    for row in &index.rows {
        if !path_matches(root, &row.path, file) {
            continue;
        }
        let rel = workspace.relative(&row.path).display().to_string();
        let ln = line_for(&mut byte_cache, &row.path, row.byte_range.start);
        let kind = if row.fql_kind.is_empty() {
            &row.node_kind
        } else {
            &row.fql_kind
        };
        entries.push((
            row.byte_range.start,
            serde_json::json!({
                "name": row.name,
                "fql_kind": kind,
                "path": rel,
                "line": ln,
            }),
        ));
    }

    entries.sort_by_key(|(offset, _)| *offset);
    let results: Vec<Value> = entries.into_iter().map(|(_, v)| v).collect();

    Ok(serde_json::json!({
        "op":      "show_outline",
        "file":    file,
        "results": results,
    }))
}

/// `SHOW members OF 'ClassName'`
///
/// Re-parses the file containing the struct/class/enum and returns its
/// direct member declarations (fields, methods, enumerators) with their
/// 1-based line numbers.
///
/// # Errors
/// Returns an error if the file cannot be read or the AST node for the
/// type is not found.
pub fn show_members(
    def: &IndexRow,
    workspace: &Workspace,
    symbol: &str,
    lang_registry: &LanguageRegistry,
) -> Result<Value> {
    let lang = lang_registry
        .language_for_path(&def.path)
        .ok_or_else(|| anyhow!("no language for {}", def.path.display()))?;
    let config = lang.config();
    let (source, tree) = parse_file(&def.path, lang_registry)?;
    let root = tree.root_node();

    let type_node = find_type_node_by_name(root, &source, symbol, config)
        .ok_or_else(|| anyhow!("AST node for '{symbol}' not found in file"))?;

    let mut members: Vec<Value> = Vec::new();

    if let Some(body) = type_node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let ln = byte_to_line(&source, child.start_byte()) + 1;
                let ck = child.kind();
                if config.is_field_kind(ck) {
                    let text = std::str::from_utf8(&source[child.byte_range()])
                        .unwrap_or("")
                        .trim()
                        .trim_end_matches(';')
                        .to_string();
                    if !text.is_empty() {
                        members.push(serde_json::json!({
                            "fql_kind": "field",
                            "text": text,
                            "line": ln,
                        }));
                    }
                } else if config.is_function_kind(ck) {
                    // Inline method definition — show signature only.
                    let body_start = child
                        .child_by_field_name("body")
                        .map_or_else(|| child.end_byte(), |b| b.start_byte());
                    let sig = std::str::from_utf8(&source[child.start_byte()..body_start])
                        .unwrap_or("")
                        .trim_end()
                        .to_string();
                    if !sig.is_empty() {
                        members.push(serde_json::json!({
                            "fql_kind": "method",
                            "text": sig,
                            "line": ln,
                        }));
                    }
                } else if config.is_declaration_kind(ck) {
                    // Method declaration (forward declaration / pure virtual).
                    let text = std::str::from_utf8(&source[child.byte_range()])
                        .unwrap_or("")
                        .trim()
                        .trim_end_matches(';')
                        .to_string();
                    if !text.is_empty() {
                        members.push(serde_json::json!({
                            "fql_kind": "method",
                            "text": text,
                            "line": ln,
                        }));
                    }
                } else if config.is_enumerator_kind(ck)
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("");
                    if !name.is_empty() {
                        members.push(serde_json::json!({
                            "fql_kind": "enumerator",
                            "text": name,
                            "line": ln,
                        }));
                    }
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    let byte_start = type_node.start_byte();
    let start_line = type_node.start_position().row + 1;
    let end_line = type_node.end_position().row + 1;
    let path_str = workspace.relative(&def.path).display().to_string();
    Ok(serde_json::json!({
        "op":         "show_members",
        "symbol":     symbol,
        "path":       path_str,
        "start_line": start_line,
        "end_line":   end_line,
        "byte_start": byte_start,
        "members":    members,
    }))
}
