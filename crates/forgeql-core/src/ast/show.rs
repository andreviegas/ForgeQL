//! Code Exposure API — SHOW command implementations.
//!
//! These operations provide controlled, progressive code disclosure to agents
//! and developers.  Each function re-reads/re-parses the relevant source file
//! on every call (no caching beyond the OS page cache) because they are
//! inherently read-only single-symbol queries with acceptable latency.
//!
//! All entry points return a `serde_json::Value` in a consistent shape:
//! `{ "op": "<op_name>", ... }` so the executor can return them directly.

use std::path::Path;

use anyhow::{Result, anyhow};
use serde_json::Value;

use crate::{
    ast::{
        index::{IndexRow, SymbolTable},
        lang::{LanguageConfig, LanguageRegistry},
    },
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

/// Parse a source file with the appropriate tree-sitter grammar.
///
/// The language is determined by file extension via the `LanguageRegistry`.
///
/// Returns `(source_bytes, tree)`.
///
/// # Errors
/// Returns an error if no language is registered for the extension, the file
/// cannot be read, or the parser fails.
pub(crate) fn parse_file(
    path: &Path,
    lang_registry: &LanguageRegistry,
) -> Result<(Vec<u8>, tree_sitter::Tree)> {
    let lang = lang_registry
        .language_for_path(path)
        .ok_or_else(|| anyhow!("no language registered for {}", path.display()))?;
    let source = crate::workspace::file_io::read_bytes(path)?;
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .map_err(|e| anyhow!("tree-sitter language error: {e}"))?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| anyhow!("failed to parse {}", path.display()))?;
    Ok((source, tree))
}

/// Walk the tree recursively to find the nearest `function_definition` node
/// whose byte range contains `def_start`.
pub(crate) fn find_enclosing_function_def<'t>(
    root: tree_sitter::Node<'t>,
    def_start: usize,
    config: &LanguageConfig,
) -> Option<tree_sitter::Node<'t>> {
    fn walk<'t>(
        cursor: &mut tree_sitter::TreeCursor<'t>,
        def_start: usize,
        func_kinds: &[String],
    ) -> Option<tree_sitter::Node<'t>> {
        let node = cursor.node();
        // Prune: skip subtrees that cannot contain def_start.
        if !node.byte_range().contains(&def_start) {
            return None;
        }
        if func_kinds.iter().any(|s| s == node.kind()) {
            return Some(node);
        }
        // Recurse into children.
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = walk(cursor, def_start, func_kinds) {
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
    walk(&mut cursor, def_start, config.function_kinds())
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
pub(crate) fn find_function_node_for_symbol<'t>(
    root: tree_sitter::Node<'t>,
    def_start: usize,
    config: &LanguageConfig,
) -> Option<tree_sitter::Node<'t>> {
    // Inner helper: walk looking for template_declaration at def_start.
    fn walk_template<'n>(
        node: tree_sitter::Node<'n>,
        def_start: usize,
        template_kind: &str,
        func_kinds: &[String],
    ) -> Option<tree_sitter::Node<'n>> {
        // Prune: skip subtrees that start after or cannot reach def_start.
        if node.start_byte() > def_start {
            return None;
        }
        if !template_kind.is_empty()
            && node.kind() == template_kind
            && node.start_byte() == def_start
        {
            // Return the first function_definition direct child.
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i)
                    && func_kinds.iter().any(|s| s == child.kind())
                {
                    return Some(child);
                }
            }
        }
        // Recurse into children that could contain def_start.
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i)
                && (child.byte_range().contains(&def_start) || child.start_byte() == def_start)
                && let Some(found) = walk_template(child, def_start, template_kind, func_kinds)
            {
                return Some(found);
            }
        }
        None
    }

    // First try: standard enclosing-function search.
    if let Some(node) = find_enclosing_function_def(root, def_start, config) {
        return Some(node);
    }

    // Second try: template_declaration at def_start → inner function_definition.
    walk_template(
        root,
        def_start,
        config.template_declaration_kind(),
        config.function_kinds(),
    )
}

/// Find a type node (struct/class/enum) whose `name` field text equals `name`.
pub(super) fn find_type_node_by_name<'t>(
    root: tree_sitter::Node<'t>,
    source: &[u8],
    name: &str,
    config: &'_ LanguageConfig,
) -> Option<tree_sitter::Node<'t>> {
    fn walk<'t>(
        cursor: &mut tree_sitter::TreeCursor<'t>,
        source: &[u8],
        name: &str,
        type_kinds: &[String],
    ) -> Option<tree_sitter::Node<'t>> {
        let node = cursor.node();
        if type_kinds.iter().any(|s| s == node.kind())
            && let Some(name_node) = node.child_by_field_name("name")
        {
            let node_name = std::str::from_utf8(&source[name_node.byte_range()]).unwrap_or("");
            if node_name == name {
                return Some(node);
            }
        }
        if cursor.goto_first_child() {
            loop {
                if let Some(found) = walk(cursor, source, name, type_kinds) {
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
    walk(&mut cursor, source, name, config.type_kinds())
}

/// Recursively collect the byte ranges of block nodes that
/// should be collapsed (their nesting depth exceeds `max_depth`).
/// Does NOT recurse into already-collapsed nodes.
fn collect_collapsed(
    node: tree_sitter::Node<'_>,
    cs_depth: usize,
    max_depth: usize,
    out: &mut Vec<std::ops::Range<usize>>,
    block_kind: &str,
) {
    let new_depth = if node.kind() == block_kind {
        cs_depth + 1
    } else {
        cs_depth
    };
    if node.kind() == block_kind && new_depth > max_depth {
        out.push(node.byte_range());
        return; // do not recurse — inner blocks are also collapsed
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_collapsed(child, new_depth, max_depth, out, block_kind);
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
pub(super) fn emit_body_lines(
    source: &[u8],
    fn_node: tree_sitter::Node<'_>,
    max_cs_depth: usize,
    config: &LanguageConfig,
) -> Vec<(usize, String)> {
    // 1 — collect collapsed ranges, sorted by start byte.
    let mut collapsed: Vec<std::ops::Range<usize>> = Vec::new();
    collect_collapsed(
        fn_node,
        0,
        max_cs_depth,
        &mut collapsed,
        config.block_kind(),
    );
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
pub(super) fn collect_callees_walk(
    source: &[u8],
    node: tree_sitter::Node<'_>,
    out: &mut Vec<String>,
    call_kind: &str,
) {
    if node.kind() == call_kind
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
            collect_callees_walk(source, child, out, call_kind);
        }
    }
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
pub(super) fn path_matches(root: &Path, path: &Path, pattern: &str) -> bool {
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

mod body;
mod callees;
mod members;

pub use body::show_body;
pub use callees::{show_callees, show_callers, show_lines};
pub use members::{show_members, show_outline};

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
/// Returns an error if the file cannot be read.
pub fn show_context(
    def: &IndexRow,
    table: &SymbolTable,
    workspace: &Workspace,
    symbol: &str,
    context_lines: usize,
) -> Result<Value> {
    let def_path = table.path_of(def);
    let source = crate::workspace::file_io::read_bytes(def_path)?;
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

    let path_str = workspace.relative(def_path).display().to_string();
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
/// Returns an error if the file cannot be read.
pub fn show_signature(
    def: &IndexRow,
    table: &SymbolTable,
    workspace: &Workspace,
    symbol: &str,
    lang_registry: &LanguageRegistry,
) -> Result<Value> {
    let def_path = table.path_of(def);
    let lang = lang_registry
        .language_for_path(def_path)
        .ok_or_else(|| anyhow!("no language for {}", def_path.display()))?;
    let config = lang.config();
    let (source, tree) = parse_file(def_path, lang_registry)?;
    let root = tree.root_node();

    let start_line = byte_to_line(&source, def.byte_range.start) + 1;
    let node_kind = table.node_kind_of(def);
    let is_func_or_template =
        config.is_function_kind(node_kind) || config.is_template_declaration_kind(node_kind);
    let (signature, end_line) = if is_func_or_template {
        find_function_node_for_symbol(root, def.byte_range.start, config).map_or_else(
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

    let path_str = workspace.relative(def_path).display().to_string();
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
        collect_callees_walk(source, tree.root_node(), &mut callees, "call_expression");
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
        collect_callees_walk(source, tree.root_node(), &mut callees, "call_expression");
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
        collect_callees_walk(source, tree.root_node(), &mut callees, "call_expression");
        assert!(callees.is_empty(), "expected no callees, got {callees:?}");
    }
}
