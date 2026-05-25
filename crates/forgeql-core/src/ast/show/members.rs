//! `SHOW outline` and `SHOW members` implementations.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::Value;

use super::{ShowRequest, byte_to_line, find_type_node_by_name, path_matches};
use crate::{
    ast::{index::SymbolTable, lang::LanguageConfig},
    workspace::Workspace,
};

/// Returns `true` when `node` (typically a `field_declaration`) is a method
/// declaration rather than a plain data-member declaration.
///
/// In tree-sitter C++, both data members (`bool flag_;`) and method
/// declarations (`void open() override;`) are represented as
/// `field_declaration` nodes.  A method declaration always contains a
/// function-declarator node somewhere in its declarator subtree (possibly
/// nested inside a `pointer_declarator`, `reference_declarator`, etc.).
///
/// `fn_decl_kind` comes from `config.function_declarator()` so the kind
/// string is never hardcoded in generic code.
fn is_method_declaration(node: tree_sitter::Node<'_>, fn_decl_kind: &str) -> bool {
    fn has_fn_declarator(n: tree_sitter::Node<'_>, kind: &str) -> bool {
        if n.kind() == kind {
            return true;
        }
        for i in 0..n.child_count() {
            if let Some(child) = n.child(i)
                && has_fn_declarator(child, kind)
            {
                return true;
            }
        }
        false
    }
    has_fn_declarator(node, fn_decl_kind)
}

/// Classify one body-child node into its member JSON entry.
///
/// Returns `None` for non-member nodes (punctuation, access specifiers, …).
fn classify_member(
    child: tree_sitter::Node<'_>,
    source: &[u8],
    config: &LanguageConfig,
) -> Option<Value> {
    let ln = byte_to_line(source, child.start_byte()) + 1;
    let ck = child.kind();

    if config.is_field_kind(ck) {
        if is_method_declaration(child, config.function_declarator()) {
            let sig = std::str::from_utf8(&source[child.start_byte()..child.end_byte()])
                .unwrap_or("")
                .trim()
                .trim_end_matches(';');
            if sig.is_empty() {
                return None;
            }
            Some(serde_json::json!({ "fql_kind": "method", "text": sig, "line": ln }))
        } else {
            let text = std::str::from_utf8(&source[child.byte_range()])
                .unwrap_or("")
                .trim()
                .trim_end_matches(';');
            if text.is_empty() {
                return None;
            }
            Some(serde_json::json!({ "fql_kind": "field", "text": text, "line": ln }))
        }
    } else if config.is_function_kind(ck) {
        let body_start = child
            .child_by_field_name("body")
            .map_or_else(|| child.end_byte(), |b| b.start_byte());
        let sig = std::str::from_utf8(&source[child.start_byte()..body_start])
            .unwrap_or("")
            .trim_end();
        if sig.is_empty() {
            return None;
        }
        Some(serde_json::json!({ "fql_kind": "method", "text": sig, "line": ln }))
    } else if config.is_declaration_kind(ck) {
        let text = std::str::from_utf8(&source[child.byte_range()])
            .unwrap_or("")
            .trim()
            .trim_end_matches(';');
        if text.is_empty() {
            return None;
        }
        Some(serde_json::json!({ "fql_kind": "method", "text": text, "line": ln }))
    } else if config.is_enumerator_kind(ck) {
        let name_node = child.child_by_field_name("name")?;
        let name = std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("");
        if name.is_empty() {
            return None;
        }
        Some(serde_json::json!({ "fql_kind": "enumerator", "text": name, "line": ln }))
    } else {
        None
    }
}

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
        if !path_matches(root, index.path_of(row), file) {
            continue;
        }
        let row_path = index.path_of(row);
        let rel = workspace.relative(row_path).display().to_string();
        let ln = line_for(&mut byte_cache, row_path, row.byte_range.start);
        let fql = index.fql_kind_of(row);
        let nk = index.node_kind_of(row);
        let kind = if fql.is_empty() { nk } else { fql };
        entries.push((
            row.byte_range.start,
            serde_json::json!({
                "name": index.name_of(row),
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
pub fn show_members(req: &ShowRequest<'_>) -> Result<Value> {
    let lang = req
        .lang_registry
        .language_for_path(req.path)
        .ok_or_else(|| anyhow!("no language for {}", req.path.display()))?;
    let config = lang.config();
    let source = &*req.cached.source;
    let root = req.cached.tree.root_node();

    let type_node = find_type_node_by_name(root, source, req.symbol, config)
        .ok_or_else(|| anyhow!("AST node for '{}' not found in file", req.symbol))?;

    let mut members: Vec<Value> = Vec::new();

    if let Some(body) = type_node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                if let Some(m) = classify_member(cursor.node(), source, config) {
                    members.push(m);
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
    let path_str = req.workspace.relative(req.path).display().to_string();
    Ok(serde_json::json!({
        "op":         "show_members",
        "symbol":     req.symbol,
        "path":       path_str,
        "start_line": start_line,
        "end_line":   end_line,
        "byte_start": byte_start,
        "members":    members,
    }))
}
