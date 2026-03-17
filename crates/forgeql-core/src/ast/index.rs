/// AST index — flat row model with dynamic fields.
///
/// Every "interesting" tree-sitter node produces one [`IndexRow`].
/// A node is interesting if [`extract_name`] returns a name for it.
///
/// KEY RULE: Never store raw `tree_sitter::Node` references.
/// Always extract byte ranges and store `Range<usize>`.
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::ForgeError;
use crate::workspace::Workspace;

// -----------------------------------------------------------------------
// IndexRow — the universal row type
// -----------------------------------------------------------------------

/// A single indexed AST node — the universal row type.
///
/// Every named tree-sitter node produces one row.  The `fields` map contains
/// all grammar fields of the node, auto-extracted by name from the Language
/// API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexRow {
    /// Human-readable symbol name (extracted by [`extract_name`]).
    pub name: String,
    /// Raw tree-sitter node kind (e.g. `"function_definition"`,
    /// `"struct_specifier"`, `"preproc_def"`).
    pub node_kind: String,
    /// Source file path (absolute path used internally).
    pub path: PathBuf,
    /// Byte range of the full AST node in the source file.
    pub byte_range: Range<usize>,
    /// 1-based start line number of the node.
    pub line: usize,
    /// Dynamic fields extracted from tree-sitter grammar field IDs.
    /// Keys are grammar field names (e.g. `"type"`, `"body"`, `"declarator"`).
    /// Values are the source text of the first child at that field.
    pub fields: HashMap<String, String>,
}

// -----------------------------------------------------------------------
// UsageSite — cross-reference entry (unchanged from v1)
// -----------------------------------------------------------------------

/// A reference (usage) of a symbol — where an identifier token appears.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSite {
    /// Source file containing the reference.
    pub path: PathBuf,
    /// Byte range of the identifier token at this usage site.
    pub byte_range: Range<usize>,
    /// 1-based source line of the identifier token.
    ///
    /// Populated at index-build time from the tree-sitter node position.
    /// Used to make individual usage rows distinguishable in CSV output.
    pub line: usize,
}

// -----------------------------------------------------------------------
// SymbolTable
// -----------------------------------------------------------------------

/// The full index for one workspace.
///
/// `build()` parses every C/C++ source file and fills:
/// - `rows`:       all named AST nodes (functions, types, macros, etc.)
/// - `usages`:     symbol name → all identifier occurrence sites
/// - `name_index`: symbol name → row indices for O(1) name lookup
/// - `kind_index`: node kind  → row indices for fast kind filtering
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SymbolTable {
    /// All indexed AST nodes (definitions, declarations, macros, includes).
    pub rows: Vec<IndexRow>,
    /// Symbol name → all sites where the identifier text appears.
    pub usages: HashMap<String, Vec<UsageSite>>,
    /// Name → row indices lookup for O(1) access.
    name_index: HashMap<String, Vec<usize>>,
    /// Node kind → row indices for fast kind filtering.
    kind_index: HashMap<String, Vec<usize>>,
}

impl SymbolTable {
    /// Build a `SymbolTable` by parsing all C/C++ files in the workspace.
    ///
    /// This is intentionally synchronous — parsing is CPU-bound.
    ///
    /// # Errors
    /// Returns `Err` if the tree-sitter language cannot be set.
    pub fn build(workspace: &Workspace) -> Result<Self> {
        let mut table = Self::default();
        let mut parser = tree_sitter::Parser::new();

        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .map_err(|e| ForgeError::TreeSitterLanguage(e.to_string()))?;

        let cpp_extensions = ["cpp", "c", "cc", "cxx", "h", "hpp", "hxx", "ino"];

        for path in workspace.files() {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !cpp_extensions.contains(&ext) {
                continue;
            }

            match index_file(&mut parser, &path, &mut table) {
                Ok(count) => {
                    debug!(
                        path = %workspace.relative(&path).display(),
                        rows = count,
                        "indexed"
                    );
                }
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "skipping file");
                }
            }
        }

        debug!(
            rows = table.rows.len(),
            usages = table.usages.values().map(Vec::len).sum::<usize>(),
            names = table.name_index.len(),
            kinds = table.kind_index.len(),
            "index built"
        );

        Ok(table)
    }

    /// Append a row and update the secondary indexes.
    pub fn push_row(&mut self, row: IndexRow) {
        let index = self.rows.len();
        self.name_index
            .entry(row.name.clone())
            .or_default()
            .push(index);
        self.kind_index
            .entry(row.node_kind.clone())
            .or_default()
            .push(index);
        self.rows.push(row);
    }

    /// Record a usage site for `name` at `byte_range` / `line` in `path`.
    pub fn add_usage(&mut self, name: String, path: &Path, byte_range: Range<usize>, line: usize) {
        self.usages.entry(name).or_default().push(UsageSite {
            path: path.to_path_buf(),
            byte_range,
            line,
        });
    }

    /// Look up all usage sites for a symbol name.
    #[must_use]
    pub fn find_usages(&self, name: &str) -> &[UsageSite] {
        self.usages.get(name).map_or(&[], Vec::as_slice)
    }

    /// Look up the primary definition row for a symbol by name.
    ///
    /// When multiple rows share a name, returns the last-indexed row
    /// (last-write-wins, matching v1 behaviour).
    #[must_use]
    pub fn find_def(&self, name: &str) -> Option<&IndexRow> {
        self.name_index
            .get(name)?
            .last()
            .map(|&idx| &self.rows[idx])
    }

    /// Return an iterator over all rows matching a tree-sitter node kind.
    pub fn rows_by_kind(&self, kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.kind_index
            .get(kind)
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i]))
    }

    // -------------------------------------------------------------------
    // Incremental update
    // -------------------------------------------------------------------

    /// Remove all entries associated with `path` and rebuild secondary indexes.
    pub fn purge_file(&mut self, path: &Path) {
        self.rows.retain(|row| row.path != path);

        // Rebuild secondary indexes from scratch.
        self.name_index.clear();
        self.kind_index.clear();
        for (index, row) in self.rows.iter().enumerate() {
            self.name_index
                .entry(row.name.clone())
                .or_default()
                .push(index);
            self.kind_index
                .entry(row.node_kind.clone())
                .or_default()
                .push(index);
        }

        for sites in self.usages.values_mut() {
            sites.retain(|usage| usage.path != path);
        }
        self.usages.retain(|_, sites| !sites.is_empty());
    }

    /// Purge and re-index a batch of files.
    ///
    /// # Errors
    /// Returns `Err` if the tree-sitter language cannot be set or any file
    /// fails to parse.
    pub fn reindex_files(&mut self, paths: &[PathBuf]) -> Result<()> {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .map_err(|e| ForgeError::TreeSitterLanguage(e.to_string()))?;

        for path in paths {
            self.purge_file(path);
            if path.exists() {
                match index_file(&mut parser, path, self) {
                    Ok(count) => {
                        debug!(path = %path.display(), rows = count, "reindexed");
                    }
                    Err(err) => {
                        warn!(path = %path.display(), error = %err, "reindex failed");
                    }
                }
            } else {
                debug!(path = %path.display(), "purged (file deleted)");
            }
        }

        Ok(())
    }
}

// -----------------------------------------------------------------------
// Index one file
// -----------------------------------------------------------------------

/// Index a single file, adding its rows to `table`.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
pub fn index_file(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    table: &mut SymbolTable,
) -> Result<usize> {
    let source = crate::workspace::file_io::read_bytes(path)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: path.to_path_buf(),
        })?;

    let language: tree_sitter::Language = tree_sitter_cpp::LANGUAGE.into();
    let before = table.rows.len();

    let mut cursor = tree.root_node().walk();
    collect_nodes(&source, path, &mut cursor, &language, "cpp", table);

    Ok(table.rows.len() - before)
}

// -----------------------------------------------------------------------
// Generic node collector
// -----------------------------------------------------------------------

/// Walk the AST and produce index rows for every named node.
///
/// A node is "interesting" if [`extract_name`] returns a name for it.
/// Identifier tokens are also indexed as usage sites regardless of kind.
///
/// `preproc_else` and `preproc_elif` subtrees are skipped entirely so that
/// only the primary (#if) branch is indexed.  Without this, tree-sitter's
/// full-source parse would create duplicate rows and usage sites for every
/// symbol that appears in both a `#if` branch and its `#else` counterpart.
fn collect_nodes(
    source: &[u8],
    path: &Path,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    language: &tree_sitter::Language,
    language_name: &str,
    table: &mut SymbolTable,
) {
    let node = cursor.node();

    // Skip alternate conditional-compilation branches to avoid duplicate rows.
    if matches!(node.kind(), "preproc_else" | "preproc_elif") {
        return;
    }

    // Every named node becomes a row.
    if let Some(name) = extract_name(node, source, language_name) {
        let fields = extract_fields(node, source, language);
        let row = IndexRow {
            name,
            node_kind: node.kind().to_string(),
            path: path.to_path_buf(),
            byte_range: node.byte_range(),
            line: node.start_position().row + 1,
            fields,
        };
        table.push_row(row);
    }

    // All identifier tokens become usage sites.
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier"
    ) {
        let name = node_text(source, node);
        if name.len() > 1 {
            let line = node.start_position().row + 1;
            table.add_usage(name, path, node.byte_range(), line);
        }
    }

    // Recurse into children.
    if cursor.goto_first_child() {
        loop {
            collect_nodes(source, path, cursor, language, language_name, table);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        let _ = cursor.goto_parent();
    }
}

// -----------------------------------------------------------------------
// Language-specific name extraction — the only grammar-specific code
// -----------------------------------------------------------------------

/// Extract the human-readable name from an AST node.
///
/// Returns `None` for nodes that should not produce index rows.
fn extract_name(node: tree_sitter::Node<'_>, source: &[u8], language_name: &str) -> Option<String> {
    // Universal: most grammars expose a "name" field on definition nodes.
    if let Some(name_node) = node.child_by_field_name("name") {
        let text = node_text(source, name_node);
        if !text.is_empty() {
            return Some(text);
        }
    }

    // Language-specific fallbacks.
    match (language_name, node.kind()) {
        // C++ function definitions: name lives inside the declarator tree.
        ("cpp", "function_definition") => node
            .child_by_field_name("declarator")
            .and_then(find_function_name)
            .map(|n| node_text(source, n))
            .filter(|s| !s.is_empty()),

        // `#include` directives: extract the path, strip surrounding delimiters.
        ("cpp", "preproc_include") => node
            .child_by_field_name("path")
            .map(|n| {
                node_text(source, n)
                    .trim_matches(|c: char| c == '"' || c == '<' || c == '>')
                    .to_string()
            })
            .filter(|s| !s.is_empty()),

        // Comments: the node text IS the name — enabling text search via
        //   FIND symbols WHERE node_kind = 'comment' WHERE name LIKE '%keyword%'
        // Both `// line comments` and `/* block comments */` use this node kind.
        ("cpp", "comment") => {
            let text = node_text(source, node);
            if text.is_empty() { None } else { Some(text) }
        }

        _ => None,
    }
}

/// Extract all grammar fields from a tree-sitter node into a string map.
fn extract_fields(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    language: &tree_sitter::Language,
) -> HashMap<String, String> {
    let mut fields = HashMap::new();
    let field_count = language.field_count();

    #[allow(clippy::cast_possible_truncation)]
    for field_id in 1..=(field_count as u16) {
        if let Some(child) = node.child_by_field_id(field_id)
            && let Some(field_name) = language.field_name_for_id(field_id)
        {
            let text = node_text(source, child);
            if !text.is_empty() {
                drop(fields.insert(field_name.to_string(), text));
            }
        }
    }

    fields
}

// -----------------------------------------------------------------------
// LANG(cpp) — function name extraction helper
// -----------------------------------------------------------------------

/// Drill into nested declarators to find the identifier holding the function name.
fn find_function_name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    match node.kind() {
        "identifier" | "destructor_name" | "operator_name" => Some(node),
        "qualified_identifier" => node
            .child_by_field_name("name")
            .and_then(find_function_name)
            .or(Some(node)),
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "abstract_function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(find_function_name),
        _ => {
            for i in 0..node.named_child_count() {
                if let Some(found) = node.named_child(i).and_then(find_function_name) {
                    return Some(found);
                }
            }
            None
        }
    }
}

// -----------------------------------------------------------------------
// Shared utilities
// -----------------------------------------------------------------------

/// Return the source text of `node` as a `String`.
pub(crate) fn node_text(source: &[u8], node: tree_sitter::Node<'_>) -> String {
    std::str::from_utf8(&source[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn two_row_table() -> SymbolTable {
        let mut table = SymbolTable::default();
        table.push_row(IndexRow {
            name: "foo".to_string(),
            node_kind: "function_definition".to_string(),
            path: PathBuf::from("a.cpp"),
            byte_range: 0..30,
            line: 1,
            fields: HashMap::new(),
        });
        table.push_row(IndexRow {
            name: "bar".to_string(),
            node_kind: "function_definition".to_string(),
            path: PathBuf::from("b.cpp"),
            byte_range: 0..30,
            line: 1,
            fields: HashMap::new(),
        });
        table.add_usage("foo".to_string(), Path::new("a.cpp"), 0..3, 1);
        table.add_usage("foo".to_string(), Path::new("b.cpp"), 10..13, 1);
        table
    }

    #[test]
    fn push_row_updates_secondary_indexes() {
        let mut table = SymbolTable::default();
        table.push_row(IndexRow {
            name: "alpha".to_string(),
            node_kind: "function_definition".to_string(),
            path: PathBuf::from("src/alpha.cpp"),
            byte_range: 0..10,
            line: 1,
            fields: HashMap::new(),
        });
        assert_eq!(table.rows.len(), 1);
        assert_eq!(table.name_index["alpha"], vec![0usize]);
        assert_eq!(table.kind_index["function_definition"], vec![0usize]);
    }

    #[test]
    fn find_def_returns_last_row_for_name() {
        let mut table = SymbolTable::default();
        table.push_row(IndexRow {
            name: "foo".to_string(),
            node_kind: "declaration".to_string(),
            path: PathBuf::from("inc/foo.h"),
            byte_range: 0..10,
            line: 1,
            fields: HashMap::new(),
        });
        table.push_row(IndexRow {
            name: "foo".to_string(),
            node_kind: "function_definition".to_string(),
            path: PathBuf::from("src/foo.cpp"),
            byte_range: 0..50,
            line: 1,
            fields: HashMap::new(),
        });
        let def = table.find_def("foo").expect("should find foo");
        assert_eq!(def.node_kind, "function_definition");
    }

    #[test]
    fn rows_by_kind_returns_correct_subset() {
        let table = two_row_table();
        let fns: Vec<&IndexRow> = table.rows_by_kind("function_definition").collect();
        assert_eq!(fns.len(), 2);
        assert!(fns.iter().any(|r| r.name == "foo"));
        assert!(fns.iter().any(|r| r.name == "bar"));
    }

    #[test]
    fn purge_file_removes_rows_and_usage_sites() {
        let mut table = two_row_table();
        table.purge_file(Path::new("a.cpp"));

        assert!(table.find_def("foo").is_none());
        assert!(table.find_def("bar").is_some());

        let foo_sites = table.find_usages("foo");
        assert_eq!(foo_sites.len(), 1);
        assert_eq!(foo_sites[0].path, PathBuf::from("b.cpp"));
    }

    #[test]
    fn purge_file_removes_empty_usage_keys() {
        let mut table = SymbolTable::default();
        table.add_usage("only_here".to_string(), Path::new("x.cpp"), 0..5, 1);
        table.purge_file(Path::new("x.cpp"));
        assert!(!table.usages.contains_key("only_here"));
    }

    #[test]
    fn reindex_files_refreshes_content() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.cpp");
        std::fs::write(&file, "void alpha() {}").unwrap();

        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .unwrap();
        index_file(&mut parser, &file, &mut table).unwrap();
        assert!(table.find_def("alpha").is_some());

        std::fs::write(&file, "void beta() {}").unwrap();
        table.reindex_files(&[file]).unwrap();

        assert!(
            table.find_def("alpha").is_none(),
            "stale entry should be purged"
        );
        assert!(table.find_def("beta").is_some(), "new entry should exist");
    }

    fn index_snippet(code: &str) -> SymbolTable {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("snippet.cpp");
        std::fs::write(&file, code).unwrap();
        let mut table = SymbolTable::default();
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .unwrap();
        index_file(&mut parser, &file, &mut table).unwrap();
        table
    }

    #[test]
    fn indexes_function_definition() {
        let table = index_snippet("void processSignal(int speed) { return; }");
        let row = table.find_def("processSignal").expect("indexed");
        assert_eq!(row.node_kind, "function_definition");
        assert_eq!(row.line, 1);
    }

    #[test]
    fn indexes_struct_specifier() {
        let table = index_snippet("struct Motor { int speed; };");
        let row = table.find_def("Motor").expect("indexed");
        assert_eq!(row.node_kind, "struct_specifier");
    }

    #[test]
    fn indexes_preproc_def() {
        let table = index_snippet("#define BAUD_RATE 9600");
        let row = table.find_def("BAUD_RATE").expect("indexed");
        assert_eq!(row.node_kind, "preproc_def");
    }

    #[test]
    fn indexes_enum_specifier() {
        let table = index_snippet("enum class State { Idle, Running };");
        let row = table.find_def("State").expect("indexed");
        assert_eq!(row.node_kind, "enum_specifier");
    }

    #[test]
    fn indexes_preproc_include() {
        let table = index_snippet("#include <stdint.h>");
        let row = table.find_def("stdint.h").expect("indexed");
        assert_eq!(row.node_kind, "preproc_include");
    }

    #[test]
    fn usage_sites_indexed_for_identifier_tokens() {
        let table = index_snippet("void foo() { foo(); }");
        let sites = table.find_usages("foo");
        assert!(!sites.is_empty(), "foo should have usage sites");
    }
}
