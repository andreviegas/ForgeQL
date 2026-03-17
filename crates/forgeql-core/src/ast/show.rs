//! Code Exposure API — SHOW command implementations.
//!
//! These operations provide controlled, progressive code disclosure to agents
//! and developers.  Each function re-reads/re-parses the relevant source file
//! on every call (no caching beyond the OS page cache) because they are
//! inherently read-only single-symbol queries with acceptable latency.
//!
//! All entry points return a `serde_json::Value` in a consistent shape:
//! `{ "op": "<op_name>", ... }` so the executor can return them directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::{
    ast::index::{IndexRow, SymbolTable},
    workspace::Workspace,
};

// -----------------------------------------------------------------------
// Internal utilities
// -----------------------------------------------------------------------

/// Convert a byte offset to a **0-based** line number by counting newlines.
#[allow(clippy::naive_bytecount)]
pub(crate) fn byte_to_line(source: &[u8], byte_offset: usize) -> usize {
    source[..byte_offset.min(source.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// Parse a C/C++ source file with tree-sitter.
///
/// Returns `(source_bytes, tree)`.
///
/// # Errors
/// Returns an error if the file cannot be read or the parser fails.
pub(crate) fn parse_cpp_file(path: &Path) -> Result<(Vec<u8>, tree_sitter::Tree)> {
    let source = crate::workspace::file_io::read_bytes(path)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|e| anyhow!("tree-sitter language error: {e}"))?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("failed to parse {}", path.display()))?;
    Ok((source, tree))
}

/// Walk the tree recursively to find the nearest `function_definition` node
/// whose byte range contains `def_start`.
pub(crate) fn find_enclosing_function_def(
    root: tree_sitter::Node<'_>,
    def_start: usize,
) -> Option<tree_sitter::Node<'_>> {
    fn walk<'t>(
        cursor: &mut tree_sitter::TreeCursor<'t>,
        def_start: usize,
    ) -> Option<tree_sitter::Node<'t>> {
        let node = cursor.node();
        // Prune: skip subtrees that cannot contain def_start.
        if !node.byte_range().contains(&def_start) {
            return None;
        }
        if node.kind() == "function_definition" {
            return Some(node);
        }
        // Recurse into children.
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = walk(cursor, def_start) {
                    // Reset cursor to the parent before returning so the
                    // caller's cursor is not left in an indeterminate state.
                    while cursor.goto_parent() {}
                    return Some(found);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            let _ = cursor.goto_parent();
        }
        None
    }

    let mut cursor = root.walk();
    walk(&mut cursor, def_start)
}

/// Locate the `function_definition` node for a symbol, with template fallback.
///
/// For regular functions, delegates to `find_enclosing_function_def`.
/// For **template functions** (where the index stores the byte start of the
/// outer `template_declaration`), `find_enclosing_function_def` fails because
/// the `function_definition` nested inside the template starts at a later
/// byte offset than `def_start`.  This function handles that case by looking
/// for a `template_declaration` whose start byte equals `def_start` and
/// returning its inner `function_definition` child.
pub(crate) fn find_function_node_for_symbol(
    root: tree_sitter::Node<'_>,
    def_start: usize,
) -> Option<tree_sitter::Node<'_>> {
    // Inner helper: walk looking for template_declaration at def_start.
    fn walk_template(
        node: tree_sitter::Node<'_>,
        def_start: usize,
    ) -> Option<tree_sitter::Node<'_>> {
        // Prune: skip subtrees that start after or cannot reach def_start.
        if node.start_byte() > def_start {
            return None;
        }
        if node.kind() == "template_declaration" && node.start_byte() == def_start {
            // Return the first function_definition direct child.
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i)
                    && child.kind() == "function_definition"
                {
                    return Some(child);
                }
            }
        }
        // Recurse into children that could contain def_start.
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i)
                && (child.byte_range().contains(&def_start) || child.start_byte() == def_start)
                && let Some(found) = walk_template(child, def_start)
            {
                return Some(found);
            }
        }
        None
    }

    // First try: standard enclosing-function search.
    if let Some(node) = find_enclosing_function_def(root, def_start) {
        return Some(node);
    }

    // Second try: template_declaration at def_start → inner function_definition.
    walk_template(root, def_start)
}

/// Find a `struct_specifier`, `class_specifier`, or `enum_specifier` node
/// whose `name` field text equals `name`.
fn find_type_node_by_name<'t>(
    root: tree_sitter::Node<'t>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'t>> {
    fn walk<'t>(
        cursor: &mut tree_sitter::TreeCursor<'t>,
        source: &[u8],
        name: &str,
    ) -> Option<tree_sitter::Node<'t>> {
        let node = cursor.node();
        if matches!(
            node.kind(),
            "struct_specifier" | "class_specifier" | "enum_specifier"
        ) && let Some(name_node) = node.child_by_field_name("name")
        {
            let node_name = std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("");
            if node_name == name {
                return Some(node);
            }
        }
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = walk(cursor, source, name) {
                    while cursor.goto_parent() {}
                    return Some(found);
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            let _ = cursor.goto_parent();
        }
        None
    }

    let mut cursor = root.walk();
    walk(&mut cursor, source, name)
}

/// Recursively collect the byte ranges of `compound_statement` nodes that
/// should be collapsed (their nesting depth exceeds `max_depth`).
/// Does NOT recurse into already-collapsed nodes.
fn collect_collapsed(
    node: tree_sitter::Node<'_>,
    cs_depth: usize,
    max_depth: usize,
    out: &mut Vec<std::ops::Range<usize>>,
) {
    let new_depth = if node.kind() == "compound_statement" {
        cs_depth + 1
    } else {
        cs_depth
    };
    if node.kind() == "compound_statement" && new_depth > max_depth {
        out.push(node.byte_range());
        return; // do not recurse — inner blocks are also collapsed
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_collapsed(child, new_depth, max_depth, out);
        }
    }
}

/// Produce a structured list of `(file_line_1based, rendered_text)` pairs
/// for the function node, with compound statements deeper than `max_cs_depth`
/// collapsed to a single `{ }` line.
///
/// - The opening `{` line becomes `<before_text>{ }<tail>` where `<tail>` is
///   any text that follows the closing `}` on the last line of the block
///   (e.g. ` while (cond);` for do-while).
/// - All source lines inside the collapsed block are omitted.
/// - Every entry in the returned vec carries its true file-absolute line
///   number so the caller can display them directly.
fn emit_body_lines(
    source: &[u8],
    fn_node: tree_sitter::Node<'_>,
    max_cs_depth: usize,
) -> Vec<(usize, String)> {
    // 1 — collect collapsed ranges, sorted by start byte.
    let mut collapsed: Vec<std::ops::Range<usize>> = Vec::new();
    collect_collapsed(fn_node, 0, max_cs_depth, &mut collapsed);
    collapsed.sort_by_key(|r| r.start);

    let fn_start = fn_node.start_byte();
    let fn_end = fn_node.end_byte();
    let fn_text = std::str::from_utf8(&source[fn_start..fn_end]).unwrap_or("");
    let fn_start_line_0 = byte_to_line(source, fn_start); // 0-based

    // 2 — build a per-line index: (byte_start_in_full_source, line_text).
    //     We split on '\n' so byte arithmetic stays simple.  Strip trailing \r.
    //     Each entry is (byte_start, text_slice).
    let mut line_infos: Vec<(usize, &str)> = Vec::new();
    let mut byte_pos = fn_start;
    for raw in fn_text.split('\n') {
        let text = raw.trim_end_matches('\r');
        line_infos.push((byte_pos, text));
        byte_pos += raw.len() + 1; // +1 for the '\n' that split() consumed
    }

    // 3 — walk lines, applying collapse rules.
    let mut result: Vec<(usize, String)> = Vec::new();
    let mut skip_until: usize = 0; // skip lines whose byte_start < this value

    'line_loop: for (i, &(li_start, li_text)) in line_infos.iter().enumerate() {
        let file_line = fn_start_line_0 + i + 1; // 1-based
        let line_end = li_start + li_text.len();

        // Skip lines fully inside a collapsed block.
        if li_start < skip_until {
            continue;
        }

        // Check whether a collapsed block's '{' starts on this line.
        for range in &collapsed {
            if range.start >= li_start && range.start <= line_end {
                // Text before '{' on this line.
                let offset_in_line = range.start - li_start;
                let before = &li_text[..offset_in_line.min(li_text.len())];

                // Text after '}' on the closing line (e.g. " while (cond);").
                let tail = line_infos
                    .iter()
                    .find(|(ls, lt)| {
                        let lend = ls + lt.len();
                        *ls <= range.end && range.end <= lend + 1
                    })
                    .map_or("", |(ls, lt)| {
                        let off = (range.end - ls).min(lt.len());
                        lt[off..].trim_end()
                    });

                let rendered = if tail.is_empty() {
                    format!("{before}{{ }}")
                } else {
                    format!("{before}{{ }} {}", tail.trim_start())
                };
                result.push((file_line, rendered));
                skip_until = range.end; // skip through closing '}'
                continue 'line_loop;
            }
        }

        result.push((file_line, li_text.to_string()));
    }

    result
}

/// Recursively collect function/method names from `call_expression` nodes
/// within `node`.  Results are appended to `out`.
///
/// Uses `child_count()` / `child(i)` instead of a `TreeCursor` to avoid
/// any cursor-state issues when starting from a non-root node.
fn collect_callees_walk(source: &[u8], node: tree_sitter::Node<'_>, out: &mut Vec<String>) {
    if node.kind() == "call_expression"
        && let Some(fn_node) = node.child_by_field_name("function")
    {
        let raw = std::str::from_utf8(&source[fn_node.byte_range()]).unwrap_or("");
        // Strip object/pointer prefix: `obj.method` → `method`,
        // `ptr->method` → `method`; `ns::fn` stays as-is.
        let callee = raw
            .rsplit('.')
            .next()
            .and_then(|s| s.rsplit("->").next())
            .unwrap_or(raw)
            .trim()
            .to_string();
        if !callee.is_empty() {
            out.push(callee);
        }
    }
    // Visit every child (named and unnamed) to catch all nested call sites.
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_callees_walk(source, child, out);
        }
    }
}

/// Look up a symbol definition, optionally requiring its path to contain
/// `file_filter` as a substring after stripping surrounding quotes.
fn find_def<'a>(
    index: &'a SymbolTable,
    symbol: &str,
    file_filter: Option<&str>,
) -> Result<&'a IndexRow> {
    let def = index
        .find_def(symbol)
        .ok_or_else(|| anyhow!("symbol '{symbol}' not found in index"))?;
    if let Some(filter) = file_filter {
        let filter_clean = filter.trim_matches('\'');
        let path_str = def.path.to_string_lossy();
        if !path_str.contains(filter_clean) {
            return Err(anyhow!(
                "symbol '{symbol}' definition is in '{path_str}', \
                 which does not match file filter '{filter_clean}'"
            ));
        }
    }
    Ok(def)
}

/// Extract the text of the source line containing `byte_offset`.
fn extract_line_at(source: &[u8], byte_offset: usize) -> String {
    let text = String::from_utf8_lossy(source);
    let clamped = byte_offset.min(text.len());
    let before = &text[..clamped];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let after = &text[line_start..];
    let line_end = after.find('\n').map_or(after.len(), |i| i);
    after[..line_end].trim().to_string()
}

/// Check whether `path` ends with the given glob-like pattern.
///
/// A `*` anywhere is treated as a "match anything" wildcard using the `ignore`
/// crate's override builder.  A literal string is matched as a path suffix.
fn path_matches(root: &Path, path: &Path, pattern: &str) -> bool {
    let rel = path.strip_prefix(root).unwrap_or(path).to_string_lossy();
    // Simple suffix match for non-glob patterns (most common case).
    if !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[') {
        let norm = pattern.replace('\\', "/");
        return rel.ends_with(&*norm) || rel == norm.trim_start_matches('/');
    }
    // Glob pattern: use ignore::overrides.
    let mut ob = ignore::overrides::OverrideBuilder::new(root);
    if ob.add(pattern).is_err() {
        return rel.contains(pattern);
    }
    ob.build()
        .map(|ov| ov.matched(&*rel, false).is_whitelist())
        .unwrap_or(false)
}

// -----------------------------------------------------------------------
// Public API
// -----------------------------------------------------------------------

/// `SHOW context OF 'symbol' [IN 'file'] [LINES n]`
///
/// Returns the source lines around the symbol's definition, ±`context_lines`
/// lines on each side (default 5).  An optional `file_filter` restricts
/// which definition file is selected when the symbol is defined in multiple
/// files (rare for C++).
///
/// # Errors
/// Returns an error if the symbol is not indexed or the file cannot be read.
pub fn show_context(
    index: &SymbolTable,
    workspace: &Workspace,
    symbol: &str,
    file_filter: Option<&str>,
    context_lines: usize,
) -> Result<Value> {
    let def = find_def(index, symbol, file_filter)?;
    let source = crate::workspace::file_io::read_bytes(&def.path)?;
    let center = byte_to_line(&source, def.byte_range.start);

    let text = String::from_utf8_lossy(&source);
    let all_lines: Vec<&str> = text.lines().collect();
    let start = center.saturating_sub(context_lines);
    let end = (center + context_lines + 1).min(all_lines.len());
    let lines: Vec<Value> = (start..end)
        .map(|i| {
            serde_json::json!({
                "line": i + 1,
                "text": all_lines[i],
                "marker": if i == center { ">>>" } else { "   " },
            })
        })
        .collect();

    let path_str = workspace.relative(&def.path).display().to_string();
    Ok(serde_json::json!({
        "op":          "show_context",
        "symbol":      symbol,
        "path":        path_str,
        "start_line":  start + 1,
        "end_line":    end,
        "center_line": center + 1,
        "byte_start":  def.byte_range.start,
        "lines":       lines,
    }))
}

/// `SHOW signature OF 'symbol'`
///
/// Returns the declaration text up to (not including) the body `{` or the
/// trailing `;`.  For functions, this is the full function header.  For
/// types, it is the first line of the declaration.
///
/// # Errors
/// Returns an error if the symbol is not indexed or the file cannot be read.
pub fn show_signature(index: &SymbolTable, workspace: &Workspace, symbol: &str) -> Result<Value> {
    let def = find_def(index, symbol, None)?;
    let (source, tree) = parse_cpp_file(&def.path)?;
    let root = tree.root_node();

    let start_line = byte_to_line(&source, def.byte_range.start) + 1;
    let (signature, end_line) = if matches!(
        def.node_kind.as_str(),
        "function_definition" | "template_declaration"
    ) {
        find_function_node_for_symbol(root, def.byte_range.start).map_or_else(
            || {
                let sig = extract_line_at(&source, def.byte_range.start);
                (sig, start_line)
            },
            |fn_node| {
                // Emit text up to (not including) the body compound_statement.
                let body_start = fn_node
                    .child_by_field_name("body")
                    .map_or_else(|| fn_node.end_byte(), |b| b.start_byte());
                let sig = std::str::from_utf8(&source[fn_node.start_byte()..body_start])
                    .unwrap_or("")
                    .trim_end()
                    .to_string();
                // The signature ends on the line just before the opening `{`.
                let sig_end_line = byte_to_line(&source, body_start.saturating_sub(1)) + 1;
                (sig, sig_end_line)
            },
        )
    } else {
        // For types/variables: one line of context is sufficient.
        let sig = extract_line_at(&source, def.byte_range.start);
        (sig, start_line)
    };

    let path_str = workspace.relative(&def.path).display().to_string();
    Ok(serde_json::json!({
        "op":         "show_signature",
        "symbol":     symbol,
        "path":       path_str,
        "start_line": start_line,
        "end_line":   end_line,
        "line":       start_line,
        "byte_start": def.byte_range.start,
        "signature":  signature,
    }))
}

/// `SHOW outline OF 'file'`
///
/// Returns all indexed symbols (functions, variables, types, macros, enums)
/// in the file, sorted by byte offset, each with its kind and 1-based line
/// number.  The `file` argument may be a glob pattern or a path suffix.
///
/// # Errors
/// Returns an error only if neither `index` nor workspace can be accessed
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
        entries.push((
            row.byte_range.start,
            serde_json::json!({
                "name": row.name,
                "kind": row.node_kind,
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
/// Returns an error if the symbol is not in the index, the file cannot be
/// read, or the AST node for the type is not found.
pub fn show_members(index: &SymbolTable, workspace: &Workspace, symbol: &str) -> Result<Value> {
    // Resolve path from the index.
    let path = index
        .find_def(symbol)
        .map(|r| r.path.clone())
        .ok_or_else(|| anyhow!("symbol '{symbol}' not found in index"))?;

    let (source, tree) = parse_cpp_file(&path)?;
    let root = tree.root_node();

    let type_node = find_type_node_by_name(root, &source, symbol)
        .ok_or_else(|| anyhow!("AST node for '{symbol}' not found in file"))?;

    let mut members: Vec<Value> = Vec::new();

    if let Some(body) = type_node.child_by_field_name("body") {
        let mut cursor = body.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let ln = byte_to_line(&source, child.start_byte()) + 1;
                match child.kind() {
                    "field_declaration" => {
                        let text = std::str::from_utf8(&source[child.byte_range()])
                            .unwrap_or("")
                            .trim()
                            .trim_end_matches(';')
                            .to_string();
                        if !text.is_empty() {
                            members.push(serde_json::json!({
                                "kind": "field",
                                "text": text,
                                "line": ln,
                            }));
                        }
                    }
                    "function_definition" => {
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
                                "kind": "method",
                                "text": sig,
                                "line": ln,
                            }));
                        }
                    }
                    "declaration" => {
                        // Method declaration (forward declaration / pure virtual).
                        let text = std::str::from_utf8(&source[child.byte_range()])
                            .unwrap_or("")
                            .trim()
                            .trim_end_matches(';')
                            .to_string();
                        if !text.is_empty() {
                            members.push(serde_json::json!({
                                "kind": "method",
                                "text": text,
                                "line": ln,
                            }));
                        }
                    }
                    "enumerator" => {
                        if let Some(name_node) = child.child_by_field_name("name") {
                            let name =
                                std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("");
                            if !name.is_empty() {
                                members.push(serde_json::json!({
                                    "kind": "enumerator",
                                    "text": name,
                                    "line": ln,
                                }));
                            }
                        }
                    }
                    _ => {}
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
    let path_str = workspace.relative(&path).display().to_string();
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

/// `SHOW body OF 'func' [DEPTH n]`
///
/// Returns the function definition as a structured `lines` array
/// `[{"line": N, "text": "..."}]` where every `line` is the file-absolute
/// 1-based line number.
///
/// - `DEPTH` absent → full body, all lines included
/// - `DEPTH 0` → signature only (lines up to but not including the `{`)
/// - `DEPTH n` (n ≥ 1) → body with compound statements at depth > n
///   collapsed to `{ }` on their opening line; inner lines are omitted
///
/// # Errors
/// Returns an error if the symbol is not indexed, the file cannot be parsed,
/// or the AST node for the function is not found.
pub fn show_body(
    index: &SymbolTable,
    workspace: &Workspace,
    symbol: &str,
    depth: Option<usize>,
) -> Result<Value> {
    let def = find_def(index, symbol, None)?;
    let (source, tree) = parse_cpp_file(&def.path)?;
    let fn_node = find_function_node_for_symbol(tree.root_node(), def.byte_range.start)
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
        Some(n) => emit_body_lines(&source, fn_node, n)
            .into_iter()
            .map(|(ln, text)| serde_json::json!({ "line": ln, "text": text }))
            .collect(),
    };

    let fn_end_line = fn_node.end_position().row + 1;
    let path_str = workspace.relative(&def.path).display().to_string();
    Ok(serde_json::json!({
        "op":         "show_body",
        "symbol":     symbol,
        "path":       path_str,
        "start_line": fn_start_line + 1,
        "end_line":   fn_end_line,
        "line":       fn_start_line + 1,
        "byte_start": fn_start,
        "depth":      depth,
        "lines":      lines,
    }))
}

/// `SHOW callers OF 'func'`
///
/// Returns all reference sites for `symbol` from the usage index, with
/// their file path and 1-based line number.
///
/// Note: the index does not distinguish call sites from type references or
/// identifier appearances.  The full reference list is returned.  Agents
/// should treat this as an upper bound on actual callers.
///
/// # Errors
/// This function currently always returns `Ok`.
pub fn show_callers(index: &SymbolTable, workspace: &Workspace, symbol: &str) -> Result<Value> {
    let sites = index.usages.get(symbol).map_or(&[] as &[_], Vec::as_slice);

    // Cache file bytes to compute line numbers without per-site file reads.
    let mut byte_cache: HashMap<PathBuf, Vec<u8>> = HashMap::new();

    let results: Vec<Value> = sites
        .iter()
        .map(|s| {
            let path_str = workspace.relative(&s.path).display().to_string();
            let src = byte_cache.entry(s.path.clone()).or_insert_with(|| {
                crate::workspace::file_io::read_bytes(&s.path).unwrap_or_default()
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
/// Returns an error if the symbol is not indexed, the file cannot be parsed,
/// or the AST node for the function is not found.
pub fn show_callees(index: &SymbolTable, workspace: &Workspace, symbol: &str) -> Result<Value> {
    let def = find_def(index, symbol, None)?;
    let (source, tree) = parse_cpp_file(&def.path)?;
    let fn_node = find_function_node_for_symbol(tree.root_node(), def.byte_range.start)
        .ok_or_else(|| anyhow!("function definition for '{symbol}' not found in AST"))?;

    let mut callees: Vec<String> = Vec::new();
    collect_callees_walk(&source, fn_node, &mut callees);
    callees.sort();
    callees.dedup();

    let path_str = workspace.relative(&def.path).display().to_string();
    let results: Vec<Value> = callees
        .iter()
        .map(|name| serde_json::json!({ "name": name }))
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
    let path = workspace.root().join(file);
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

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::index::SymbolTable;

    fn empty_index() -> SymbolTable {
        SymbolTable::default()
    }

    #[test]
    fn byte_to_line_basic() {
        let src = b"line1\nline2\nline3";
        assert_eq!(byte_to_line(src, 0), 0); // 'l' of line1 → line 0
        assert_eq!(byte_to_line(src, 6), 1); // 'l' of line2 → line 1
        assert_eq!(byte_to_line(src, 12), 2); // 'l' of line3 → line 2
    }

    #[test]
    fn show_callers_empty_index_is_ok() {
        // Even with an unknown symbol, show_callers should not panic — it
        // returns an empty results array.
        let index = empty_index();
        let ws = crate::workspace::Workspace::new(env!("CARGO_MANIFEST_DIR")).unwrap();
        let v = show_callers(&index, &ws, "nonexistent").unwrap();
        assert_eq!(v["op"], "show_callers");
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn path_matches_suffix() {
        let root = std::path::Path::new("/repo");
        assert!(path_matches(
            root,
            std::path::Path::new("/repo/src/foo.cpp"),
            "foo.cpp"
        ));
        assert!(path_matches(
            root,
            std::path::Path::new("/repo/src/foo.cpp"),
            "src/foo.cpp"
        ));
        assert!(!path_matches(
            root,
            std::path::Path::new("/repo/src/bar.cpp"),
            "foo.cpp"
        ));
    }

    #[test]
    fn extract_line_at_basic() {
        let src = b"first line\nsecond line\nthird";
        assert_eq!(extract_line_at(src, 0), "first line");
        assert_eq!(extract_line_at(src, 11), "second line");
    }

    // -----------------------------------------------------------------------
    // show_lines unit tests
    // -----------------------------------------------------------------------

    /// Build a temporary workspace containing a single file `test.txt` with
    /// the given content, then call `f` with the workspace and filename.
    fn with_tmp_file<T>(
        content: &str,
        f: impl FnOnce(&crate::workspace::Workspace, &str) -> T,
    ) -> T {
        let dir = tempfile::tempdir().expect("tempdir");
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, content).expect("write");
        let ws = crate::workspace::Workspace::new(dir.path()).unwrap();
        f(&ws, "test.txt")
    }

    #[test]
    fn show_lines_returns_correct_range() {
        with_tmp_file("alpha\nbeta\ngamma\ndelta\n", |ws, file| {
            let v = show_lines(ws, file, 2, 3).unwrap();
            assert_eq!(v["op"], "show_lines");
            assert_eq!(v["start_line"], 2);
            assert_eq!(v["end_line"], 3);
            let lines = v["lines"].as_array().unwrap();
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0]["text"], "beta");
            assert_eq!(lines[0]["line"], 2);
            assert_eq!(lines[1]["text"], "gamma");
            assert_eq!(lines[1]["line"], 3);
        });
    }

    #[test]
    fn show_lines_clamps_end_beyond_eof() {
        with_tmp_file("line1\nline2\n", |ws, file| {
            let v = show_lines(ws, file, 1, 9999).unwrap();
            // Should return all lines without error.
            assert!(v["lines"].as_array().unwrap().len() >= 2);
        });
    }

    #[test]
    fn show_lines_start_zero_is_error() {
        with_tmp_file("hello\n", |ws, file| {
            assert!(show_lines(ws, file, 0, 1).is_err());
        });
    }

    // ── BUG #3 regression: collect_callees_walk must find nested call_expression nodes ──

    #[test]
    fn collect_callees_walk_finds_function_calls() {
        // Simple C++ function that calls two other functions.
        let source = b"void setup() { Serial_begin(9600); digitalWrite(13, 0); }";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("load cpp language");
        let tree = parser.parse(source, None).expect("parse");

        let mut callees: Vec<String> = Vec::new();
        collect_callees_walk(source, tree.root_node(), &mut callees);
        callees.sort();
        callees.dedup();

        assert!(
            callees.contains(&"Serial_begin".to_string()),
            "expected Serial_begin in {callees:?}"
        );
        assert!(
            callees.contains(&"digitalWrite".to_string()),
            "expected digitalWrite in {callees:?}"
        );
    }

    #[test]
    fn collect_callees_walk_handles_member_call() {
        // C++ method call via `.` and `->` should strip the object prefix.
        let source = b"void loop() { obj.method1(); ptr->method2(); }";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("load cpp language");
        let tree = parser.parse(source, None).expect("parse");

        let mut callees: Vec<String> = Vec::new();
        collect_callees_walk(source, tree.root_node(), &mut callees);
        callees.sort();
        callees.dedup();

        assert!(
            callees.contains(&"method1".to_string()),
            "expected method1 in {callees:?}"
        );
        assert!(
            callees.contains(&"method2".to_string()),
            "expected method2 in {callees:?}"
        );
    }

    #[test]
    fn collect_callees_walk_empty_function_returns_nothing() {
        let source = b"void noop() {}";
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .expect("load cpp language");
        let tree = parser.parse(source, None).expect("parse");

        let mut callees: Vec<String> = Vec::new();
        collect_callees_walk(source, tree.root_node(), &mut callees);
        assert!(callees.is_empty(), "expected no callees, got {callees:?}");
    }
}
