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
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::ast::enrich::guard_utils::{
    GuardFrame, build_env_guard_frame, build_guard_frame, collect_attribute_guard_frames,
    inject_guard_fields,
};
use crate::ast::enrich::macro_table::MacroTable;
use crate::ast::enrich::{EnrichContext, NodeEnricher, default_enrichers};
use crate::ast::lang::{LanguageRegistry, LanguageSupport};
use crate::ast::trigram::TrigramIndex;
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
    /// Universal FQL kind (e.g. `"function"`, `"class"`, `"number"`).
    /// Empty string when no mapping exists for this `node_kind`.
    #[serde(default)]
    pub fql_kind: String,
    /// Language identifier (e.g. `"cpp"`, `"typescript"`).
    #[serde(default)]
    pub language: String,
    /// Source file path (absolute path used internally).
    pub path: PathBuf,
    /// Byte range of the full AST node in the source file.
    pub byte_range: Range<usize>,
    /// 1-based start line number of the node.
    pub line: usize,
    /// Number of times this symbol name appears as an identifier reference
    /// across the indexed workspace.  Precomputed at build time so queries
    /// can filter/sort by `usages` without a per-row `HashMap` lookup.
    #[serde(default)]
    pub usages_count: u32,
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
// IndexStats — pre-aggregated group counts
// -----------------------------------------------------------------------

/// Pre-aggregated per-group symbol counts, computed once at build time.
///
/// Enables O(1) `GROUP BY fql_kind` and `GROUP BY language` queries without
/// scanning the full row list.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct IndexStats {
    /// Symbol count per `fql_kind` value.
    pub by_fql_kind: HashMap<String, usize>,
    /// Symbol count per `language` value.
    pub by_language: HashMap<String, usize>,
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
/// - `stats`:      pre-aggregated group counts for O(1) GROUP BY
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
    /// FQL kind → row indices for fast universal-kind filtering.
    fql_kind_index: HashMap<String, Vec<usize>>,
    /// Pre-aggregated group counts for O(1) GROUP BY on `fql_kind` / `language`.
    #[serde(default)]
    pub stats: IndexStats,
    /// Trigram inverted index over symbol names for fast substring / regex pre-filtering.
    ///
    /// Not persisted in the cache — rebuilt in O(N) from `rows` during load via `push_row`.
    #[serde(skip)]
    pub trigram_index: TrigramIndex,
}

impl SymbolTable {
    /// Build a `SymbolTable` by parsing all supported files in the workspace.
    ///
    /// Files are parsed and enriched **in parallel** using rayon.  Each thread
    /// creates its own `Parser` and enricher set, producing a per-file table.
    /// Results are merged sequentially, then post-pass enrichment runs.
    ///
    /// # Errors
    /// Returns `Err` if the tree-sitter language cannot be set.
    pub fn build(
        workspace: &Workspace,
        lang_registry: &LanguageRegistry,
    ) -> Result<(Self, MacroTable)> {
        // 1 — collect file paths that have a registered language.
        let paths: Vec<PathBuf> = workspace
            .files()
            .filter(|p| lang_registry.language_for_path(p).is_some())
            .collect();

        debug!(files = paths.len(), "indexing files in parallel");

        // Pass 1 — collect macro definitions (parallel, per-file, then merged).
        let macro_table: MacroTable = paths
            .par_iter()
            .filter_map(|path| {
                let lang = lang_registry.language_for_path(path)?;
                let _ = lang.macro_expander()?;
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    return None;
                }
                match collect_macro_defs_for_file(&mut parser, path, lang.as_ref()) {
                    Ok(defs) if !defs.is_empty() => {
                        let mut local = MacroTable::new();
                        for def in defs {
                            local.insert(def);
                        }
                        Some(local)
                    }
                    _ => None,
                }
            })
            .reduce(MacroTable::new, |mut acc, local| {
                acc.merge_from(local);
                acc
            });

        debug!(
            macro_defs = macro_table.def_count(),
            "first-pass macro collection complete"
        );

        // Pass 2 — parse + enrich each file in parallel, merging via tree
        // reduction so merges also happen across multiple cores.
        let mut table: Self = paths
            .par_iter()
            .filter_map(|path| {
                let lang = lang_registry.language_for_path(path)?;
                let mut parser = tree_sitter::Parser::new();
                if parser.set_language(&lang.tree_sitter_language()).is_err() {
                    warn!(path = %path.display(), "failed to set tree-sitter language");
                    return None;
                }
                let enrichers = default_enrichers();
                let mut file_table = Self::default();

                match index_file(
                    &mut parser,
                    path,
                    &mut file_table,
                    &enrichers,
                    lang.as_ref(),
                    Some(&macro_table),
                ) {
                    Ok(count) => {
                        debug!(
                            path = %workspace.relative(path).display(),
                            rows = count,
                            "indexed"
                        );
                    }
                    Err(err) => {
                        warn!(path = %path.display(), error = %err, "skipping file");
                        return None;
                    }
                }
                Some(file_table)
            })
            .reduce(Self::default, |mut acc, file_table| {
                acc.merge(file_table);
                acc
            });

        debug!(
            rows = table.rows.len(),
            usages = table.usages.values().map(Vec::len).sum::<usize>(),
            names = table.name_index.len(),
            kinds = table.kind_index.len(),
            "index built"
        );

        // Post-pass — run post_pass for each enricher (aggregation, cross-row metrics).
        let enrichers = default_enrichers();
        for enricher in &enrichers {
            enricher.post_pass(&mut table);
        }

        // Precompute per-row usages_count from the completed usages map.
        table.populate_usage_counts();

        Ok((table, macro_table))
    }

    /// Merge another `SymbolTable` into this one.
    ///
    /// Row indices in `name_index` and `kind_index` are offset by the
    /// current row count so they remain correct after the merge.
    fn merge(&mut self, other: Self) {
        let offset = self.rows.len();

        // Merge rows and fix secondary indexes.
        for (i, row) in other.rows.into_iter().enumerate() {
            self.name_index
                .entry(row.name.clone())
                .or_default()
                .push(offset + i);
            self.kind_index
                .entry(row.node_kind.clone())
                .or_default()
                .push(offset + i);
            if !row.fql_kind.is_empty() {
                self.fql_kind_index
                    .entry(row.fql_kind.clone())
                    .or_default()
                    .push(offset + i);
                *self
                    .stats
                    .by_fql_kind
                    .entry(row.fql_kind.clone())
                    .or_insert(0) += 1;
            }
            if !row.language.is_empty() {
                *self
                    .stats
                    .by_language
                    .entry(row.language.clone())
                    .or_insert(0) += 1;
            }
            self.trigram_index.insert(offset + i, &row.name);
            self.rows.push(row);
        }

        // Merge usage sites.
        for (name, sites) in other.usages {
            self.usages.entry(name).or_default().extend(sites);
        }
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
        if !row.fql_kind.is_empty() {
            self.fql_kind_index
                .entry(row.fql_kind.clone())
                .or_default()
                .push(index);
            *self
                .stats
                .by_fql_kind
                .entry(row.fql_kind.clone())
                .or_insert(0) += 1;
        }
        if !row.language.is_empty() {
            *self
                .stats
                .by_language
                .entry(row.language.clone())
                .or_insert(0) += 1;
        }
        // Update trigram index before moving `row` into `self.rows`.
        self.trigram_index.insert(index, &row.name);
        self.rows.push(row);
    }

    /// Fill `IndexRow::usages_count` for every row from the `usages` map.
    ///
    /// Must be called after both `rows` and `usages` are fully populated.
    /// Skips rows where `usages_count` is already non-zero (idempotent on
    /// indexes built with a version that persists the field).
    pub fn populate_usage_counts(&mut self) {
        for i in 0..self.rows.len() {
            // Extract the bare name suffix (after last `::`) as an owned
            // String to release the immutable borrow on `self.rows[i]`
            // before we look up `self.usages`.
            let usages_key = {
                let n = &self.rows[i].name;
                n.rsplit("::").next().unwrap_or(n).to_owned()
            };
            let count = self
                .usages
                .get(&usages_key)
                .map_or(0, |v| u32::try_from(v.len()).unwrap_or(u32::MAX));
            self.rows[i].usages_count = count;
        }
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

    /// Return all definition rows for a given symbol name.
    ///
    /// Unlike [`find_def`] which returns only the last-indexed row,
    /// this returns every row matching the name — essential for
    /// multi-language workspaces where the same name may exist in
    /// different files/languages.
    #[must_use]
    pub fn find_all_defs(&self, name: &str) -> Vec<&IndexRow> {
        self.name_index.get(name).map_or_else(Vec::new, |indices| {
            indices.iter().map(|&idx| &self.rows[idx]).collect()
        })
    }

    /// Return up to `max` symbol names that are similar to `query`.
    ///
    /// Uses case-insensitive prefix matching and substring matching to
    /// find plausible alternatives when a symbol lookup fails.
    #[must_use]
    pub fn suggest_similar(&self, query: &str, max: usize) -> Vec<&str> {
        let lower = query.to_ascii_lowercase();
        let mut results: Vec<&str> = self
            .name_index
            .keys()
            .filter(|name| {
                let nl = name.to_ascii_lowercase();
                nl.starts_with(&lower) || lower.starts_with(&nl) || nl.contains(&lower)
            })
            .map(String::as_str)
            .take(max)
            .collect();
        results.sort_unstable();
        results.truncate(max);
        results
    }
    /// Return an iterator over all rows matching a tree-sitter node kind.
    pub fn rows_by_kind(&self, kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.kind_index
            .get(kind)
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i]))
    }

    /// Return an iterator over all rows matching a universal FQL kind.
    pub fn rows_by_fql_kind(&self, fql_kind: &str) -> impl Iterator<Item = &IndexRow> {
        self.fql_kind_index
            .get(fql_kind)
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i]))
    }

    /// Return an iterator over all rows with an exact name match.
    ///
    /// O(1) lookup via `name_index`; suitable for wildcard-free `LIKE` and
    /// fully-anchored `MATCHES` predicates.
    pub fn rows_by_name(&self, name: &str) -> impl Iterator<Item = &IndexRow> {
        self.name_index
            .get(name)
            .into_iter()
            .flat_map(|v| v.iter().map(|&i| &self.rows[i]))
    }

    /// Return candidate rows whose names contain `substr` according to the
    /// trigram index.  The result is a **superset** — callers must still
    /// verify the full predicate.  Returns `None` when `substr` is too short
    /// (< 3 bytes) to use trigrams.
    #[must_use]
    pub fn trigram_candidates(&self, substr: &str) -> Option<Vec<&IndexRow>> {
        let ids = self.trigram_index.candidates(substr)?;
        Some(ids.into_iter().map(|i| &self.rows[i]).collect())
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
        self.fql_kind_index.clear();
        self.trigram_index.clear();
        for (index, row) in self.rows.iter().enumerate() {
            self.name_index
                .entry(row.name.clone())
                .or_default()
                .push(index);
            self.kind_index
                .entry(row.node_kind.clone())
                .or_default()
                .push(index);
            if !row.fql_kind.is_empty() {
                self.fql_kind_index
                    .entry(row.fql_kind.clone())
                    .or_default()
                    .push(index);
            }
            self.trigram_index.insert(index, &row.name);
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
    pub fn reindex_files(
        &mut self,
        paths: &[PathBuf],
        lang_registry: &LanguageRegistry,
    ) -> Result<()> {
        let mut parser = tree_sitter::Parser::new();
        let enrichers = default_enrichers();

        for path in paths {
            self.purge_file(path);
            if path.exists() {
                if let Some(lang) = lang_registry.language_for_path(path) {
                    parser
                        .set_language(&lang.tree_sitter_language())
                        .map_err(|e| ForgeError::TreeSitterLanguage(e.to_string()))?;
                    match index_file(&mut parser, path, self, &enrichers, lang.as_ref(), None) {
                        Ok(count) => {
                            debug!(path = %path.display(), rows = count, "reindexed");
                        }
                        Err(err) => {
                            warn!(path = %path.display(), error = %err, "reindex failed");
                        }
                    }
                } else {
                    debug!(path = %path.display(), "purged (unsupported language)");
                }
            } else {
                debug!(path = %path.display(), "purged (file deleted)");
            }
        }

        // Run post_pass for each enricher after reindexing.
        for enricher in &enrichers {
            enricher.post_pass(self);
        }

        Ok(())
    }
}

// -----------------------------------------------------------------------
// First-pass macro collector
// -----------------------------------------------------------------------

/// Walk the AST of a single file and collect all macro definitions.
///
/// Returns an empty `Vec` when the language has no `macro_expander()`.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
fn collect_macro_defs_for_file(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    language: &dyn LanguageSupport,
) -> Result<Vec<crate::ast::lang::MacroDef>> {
    let Some(expander) = language.macro_expander() else {
        return Ok(Vec::new());
    };
    let source = crate::workspace::file_io::read_bytes(path)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: path.to_path_buf(),
        })?;
    let config = language.config();
    let mut cursor = tree.root_node().walk();
    let mut defs = Vec::new();
    loop {
        let node = cursor.node();
        if config.macro_def_kinds().iter().any(|k| k == node.kind())
            && let Some(mut def) = expander.extract_def(node, &source, config)
        {
            def.file = path.to_path_buf();
            defs.push(def);
        }
        if !config.is_skip_kind(node.kind()) && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return Ok(defs);
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

// -----------------------------------------------------------------------
// Index one file (second pass)
// -----------------------------------------------------------------------

/// Index a single file, adding its rows to `table`.
///
/// `macro_table` — optional table of macro definitions built during the
/// first pass; passed through to [`EnrichContext`] for macro-aware enrichers.
///
/// # Errors
/// Returns an error if the file cannot be read or tree-sitter parsing fails.
pub fn index_file(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    table: &mut SymbolTable,
    enrichers: &[Box<dyn NodeEnricher>],
    language: &dyn LanguageSupport,
    macro_table: Option<&MacroTable>,
) -> Result<usize> {
    let source = crate::workspace::file_io::read_bytes(path)?;
    let tree = parser
        .parse(&source, None)
        .ok_or_else(|| ForgeError::AstParse {
            path: path.to_path_buf(),
        })?;

    let ts_lang = language.tree_sitter_language();
    let before = table.rows.len();

    let mut cursor = tree.root_node().walk();
    collect_nodes(
        &source,
        path,
        &mut cursor,
        &ts_lang,
        language,
        table,
        enrichers,
        macro_table,
    );

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
///
/// Uses iterative depth-first traversal via `TreeCursor` navigation to
/// avoid stack overflow on large codebases (e.g. Zephyr RTOS).
#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
fn collect_nodes(
    source: &[u8],
    path: &Path,
    cursor: &mut tree_sitter::TreeCursor<'_>,
    ts_language: &tree_sitter::Language,
    language: &dyn LanguageSupport,
    table: &mut SymbolTable,
    enrichers: &[Box<dyn NodeEnricher>],
    macro_table: Option<&MacroTable>,
) {
    let config = language.config();
    let lang_name = language.name();
    let mut guard_stack: Vec<GuardFrame> = Vec::new();
    // Pre-compile env_guard_patterns once per file.
    let env_guard_regex: Option<regex::RegexSet> = if config.env_guard_patterns().is_empty() {
        None
    } else {
        regex::RegexSet::new(config.env_guard_patterns()).ok()
    };

    loop {
        let node = cursor.node();

        // --- Guard stack management ---
        // Pop frames whose byte scope we've left.
        while let Some(frame) = guard_stack.last() {
            if node.start_byte() >= frame.guard_byte_range.end {
                drop(guard_stack.pop());
            } else {
                break;
            }
        }
        // Push a new frame when entering a block-guard-opening node.
        if config.has_guard_support()
            && (config.is_block_guard_kind(node.kind())
                || config.is_elif_kind(node.kind())
                || config.is_else_kind(node.kind()))
        {
            let frame = build_guard_frame(node, source, config, &guard_stack);
            guard_stack.push(frame);
        }
        // Push a heuristic guard frame for env-guarded `if` nodes
        // (e.g. Python `if TYPE_CHECKING:` or `if sys.platform == "linux":`).
        if let Some(regex_set) = &env_guard_regex
            && language.map_kind(node.kind()) == Some("if")
            && let Some(frame) = build_env_guard_frame(node, source, config, regex_set)
        {
            guard_stack.push(frame);
        }
        // --- End guard stack management ---

        // Skip alternate conditional-compilation branches entirely.
        let skip = config.is_skip_kind(node.kind());

        if !skip {
            // Build the enrichment context once for this node.
            let ctx = EnrichContext {
                node,
                source,
                path,
                language_name: lang_name,
                language_config: config,
                language_support: language,
                guard_stack: &guard_stack,
                macro_table,
            };

            // Every named node becomes a row.
            if let Some(name) = language.extract_name(node, source) {
                let mut fields = extract_fields(node, source, ts_language);

                // Inject guard fields from the current block-guard stack.
                if !guard_stack.is_empty() {
                    inject_guard_fields(&guard_stack, &mut fields);
                }

                // Inject item-level attribute guards (e.g. Rust `#[cfg(...)]`).
                let attr_guard_name = config.item_guard_attribute();
                if !attr_guard_name.is_empty() {
                    let attr_frames = collect_attribute_guard_frames(node, source, attr_guard_name);
                    if !attr_frames.is_empty() {
                        inject_guard_fields(&attr_frames, &mut fields);
                    }
                }

                // Run all enrichers on this row.
                for enricher in enrichers {
                    enricher.enrich_row(&ctx, &name, &mut fields);
                }

                let fql_kind_val = language.map_kind(node.kind()).unwrap_or("");

                let row = IndexRow {
                    name,
                    node_kind: node.kind().to_string(),
                    fql_kind: fql_kind_val.to_string(),
                    language: lang_name.to_string(),
                    path: path.to_path_buf(),
                    byte_range: node.byte_range(),
                    line: node.start_position().row + 1,
                    usages_count: 0,
                    fields,
                };
                table.push_row(row);
            } else if let Some(mtable) = macro_table {
                // Re-tag: tree-sitter-cpp parses C macro calls as
                // call_expression, not macro_invocation.  When extract_name
                // returns None for a call_expression whose function name is
                // in the MacroTable, emit a macro_call row.
                let call_kind = config.call_expression_kind();
                if !call_kind.is_empty()
                    && node.kind() == call_kind
                    && let Some(func_node) = node.child_by_field_name("function")
                {
                    let func_name = node_text(source, func_node);
                    if !func_name.is_empty() && mtable.contains(&func_name) {
                        let mut fields = extract_fields(node, source, ts_language);

                        if !guard_stack.is_empty() {
                            inject_guard_fields(&guard_stack, &mut fields);
                        }
                        let attr_guard_name = config.item_guard_attribute();
                        if !attr_guard_name.is_empty() {
                            let attr_frames =
                                collect_attribute_guard_frames(node, source, attr_guard_name);
                            if !attr_frames.is_empty() {
                                inject_guard_fields(&attr_frames, &mut fields);
                            }
                        }

                        for enricher in enrichers {
                            enricher.enrich_row(&ctx, &func_name, &mut fields);
                        }

                        let row = IndexRow {
                            name: func_name,
                            node_kind: node.kind().to_string(),
                            fql_kind: "macro_call".to_string(),
                            language: lang_name.to_string(),
                            path: path.to_path_buf(),
                            byte_range: node.byte_range(),
                            line: node.start_position().row + 1,
                            usages_count: 0,
                            fields,
                        };
                        table.push_row(row);
                    }
                }
            }
            // Run extra_rows() for every node (even if extract_name returned None).
            for enricher in enrichers {
                for row in enricher.extra_rows(&ctx) {
                    table.push_row(row);
                }
            }

            // All identifier tokens become usage sites.
            if config.is_usage_node_kind(node.kind()) {
                let name = node_text(source, node);
                if name.len() > 1 {
                    let line = node.start_position().row + 1;
                    table.add_usage(name, path, node.byte_range(), line);
                }
            }

            // Descend into children.
            if cursor.goto_first_child() {
                continue;
            }
        }
        // When `skip` is true we never call goto_first_child(), so the
        // entire subtree is skipped — matches the old early-return behaviour.

        // Move to next sibling, or walk up until we find one.
        if cursor.goto_next_sibling() {
            continue;
        }
        let mut found_sibling = false;
        while cursor.goto_parent() {
            if cursor.goto_next_sibling() {
                found_sibling = true;
                break;
            }
        }
        if !found_sibling {
            break;
        }
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
    use crate::ast::lang::CppLanguageInline;

    fn two_row_table() -> SymbolTable {
        let mut table = SymbolTable::default();
        table.push_row(IndexRow {
            name: "foo".to_string(),
            node_kind: "function_definition".to_string(),
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from("a.cpp"),
            byte_range: 0..30,
            line: 1,
            usages_count: 0,
            fields: HashMap::new(),
        });
        table.push_row(IndexRow {
            name: "bar".to_string(),
            node_kind: "function_definition".to_string(),
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from("b.cpp"),
            byte_range: 0..30,
            line: 1,
            usages_count: 0,
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
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from("src/alpha.cpp"),
            byte_range: 0..10,
            line: 1,
            usages_count: 0,
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
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from("inc/foo.h"),
            byte_range: 0..10,
            line: 1,
            usages_count: 0,
            fields: HashMap::new(),
        });
        table.push_row(IndexRow {
            name: "foo".to_string(),
            node_kind: "function_definition".to_string(),
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from("src/foo.cpp"),
            byte_range: 0..50,
            line: 1,
            usages_count: 0,
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
        index_file(
            &mut parser,
            &file,
            &mut table,
            &default_enrichers(),
            &CppLanguageInline,
            None,
        )
        .unwrap();
        assert!(table.find_def("alpha").is_some());

        std::fs::write(&file, "void beta() {}").unwrap();
        let registry = LanguageRegistry::new(vec![std::sync::Arc::new(CppLanguageInline)]);
        table.reindex_files(&[file], &registry).unwrap();

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
        index_file(
            &mut parser,
            &file,
            &mut table,
            &default_enrichers(),
            &CppLanguageInline,
            None,
        )
        .unwrap();
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
    fn indexes_qualified_function_definition() {
        let table =
            index_snippet("class Motor { void setup(); };\nvoid Motor::setup() { return; }");
        assert!(
            table.find_def("Motor::setup").is_some(),
            "qualified method should be indexed under its full name"
        );
        // The member declaration inside the class body is also indexed
        // under the bare name as a field_declaration.
        let decl = table
            .find_def("setup")
            .expect("member declaration should be indexed");
        assert_eq!(decl.node_kind, "field_declaration");
    }

    #[test]
    fn bare_and_qualified_functions_coexist() {
        let table = index_snippet(
            "void setup() {}\nclass Motor { void setup(); };\nvoid Motor::setup() {}",
        );
        // find_def returns the last row for a name — the field_declaration
        // from the class body comes after the bare function_definition.
        let last = table.find_def("setup").expect("setup");
        assert_eq!(last.node_kind, "field_declaration");
        // The bare function_definition is still in the table.
        let has_bare_def = table
            .rows
            .iter()
            .any(|r| r.name == "setup" && r.node_kind == "function_definition");
        assert!(has_bare_def, "bare function_definition should exist");
        let qualified = table.find_def("Motor::setup").expect("qualified setup");
        assert_eq!(qualified.node_kind, "function_definition");
    }

    #[test]
    fn indexes_member_function_declaration() {
        let table = index_snippet(
            "class SignalSequencer {\n  void loadSignalCode(int code);\n  int getValue() const;\n};",
        );
        let load = table
            .find_def("loadSignalCode")
            .expect("member declaration indexed");
        assert_eq!(load.node_kind, "field_declaration");
        let get = table
            .find_def("getValue")
            .expect("member declaration indexed");
        assert_eq!(get.node_kind, "field_declaration");
    }

    #[test]
    fn indexes_member_data_field() {
        let table = index_snippet("struct Point { int x; double y; };");
        let x = table.find_def("x").expect("data member indexed");
        assert_eq!(x.node_kind, "field_declaration");
        let y = table.find_def("y").expect("data member indexed");
        assert_eq!(y.node_kind, "field_declaration");
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

    #[test]
    fn member_method_declaration_carries_body_symbol() {
        let table = index_snippet(
            "class Motor { void setup(int speed); };\nvoid Motor::setup(int speed) {}",
        );
        let decl = table.find_def("setup").expect("member declaration indexed");
        assert_eq!(decl.node_kind, "field_declaration");
        assert_eq!(
            decl.fields.get("body_symbol").map(String::as_str),
            Some("Motor::setup"),
            "body_symbol must point to the qualified name"
        );
    }

    #[test]
    fn data_member_has_no_body_symbol() {
        let table = index_snippet("struct Point { int x; double y; };");
        let x = table.find_def("x").expect("data member indexed");
        assert!(
            !x.fields.contains_key("body_symbol"),
            "data members should not have body_symbol"
        );
    }
    // -- find_all_defs ---------------------------------------------------

    #[test]
    fn find_all_defs_empty_for_unknown_name() {
        let table = two_row_table();
        let defs = table.find_all_defs("nonexistent");
        assert!(defs.is_empty(), "unknown symbol must return empty vec");
    }

    #[test]
    fn find_all_defs_returns_all_matching_rows() {
        // Push the same name into two files to simulate a multi-file workspace.
        let mut table = SymbolTable::default();
        let make_row = |path: &str| IndexRow {
            name: "shared".to_string(),
            node_kind: "function_definition".to_string(),
            fql_kind: String::new(),
            language: String::new(),
            path: PathBuf::from(path),
            byte_range: 0..10,
            line: 1,
            usages_count: 0,
            fields: HashMap::new(),
        };
        table.push_row(make_row("src/a.cpp"));
        table.push_row(make_row("src/b.cpp"));

        let defs = table.find_all_defs("shared");
        assert_eq!(defs.len(), 2, "both rows must be returned");
    }

    #[test]
    fn find_all_defs_single_result() {
        let table = two_row_table();
        let defs = table.find_all_defs("foo");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "foo");
    }

    // -- suggest_similar -------------------------------------------------

    #[test]
    fn suggest_similar_prefix_match() {
        let table = two_row_table(); // has "foo" and "bar"
        let suggestions = table.suggest_similar("fo", 10);
        assert!(suggestions.contains(&"foo"), "prefix 'fo' must match 'foo'");
    }

    #[test]
    fn suggest_similar_substring_match() {
        let table = two_row_table(); // has "foo" and "bar"
        let suggestions = table.suggest_similar("oo", 10);
        assert!(
            suggestions.contains(&"foo"),
            "substring 'oo' must match 'foo'"
        );
    }

    #[test]
    fn suggest_similar_case_insensitive() {
        let table = two_row_table(); // has "foo"
        let suggestions = table.suggest_similar("FOO", 10);
        assert!(
            suggestions.contains(&"foo"),
            "uppercase query must match lowercase name"
        );
    }

    #[test]
    fn suggest_similar_no_match_returns_empty() {
        let table = two_row_table();
        let suggestions = table.suggest_similar("zzz_nonexistent", 10);
        assert!(suggestions.is_empty(), "no match must return empty vec");
    }

    #[test]
    fn suggest_similar_respects_max_limit() {
        // Build a table with 5 symbols that all start with "sym".
        let mut table = SymbolTable::default();
        for i in 0..5_usize {
            table.push_row(IndexRow {
                name: format!("sym_{i}"),
                node_kind: "function_definition".to_string(),
                fql_kind: String::new(),
                language: String::new(),
                path: PathBuf::from("src/lib.cpp"),
                byte_range: 0..10,
                line: 1,
                usages_count: 0,
                fields: HashMap::new(),
            });
        }
        let suggestions = table.suggest_similar("sym", 3);
        assert!(
            suggestions.len() <= 3,
            "result must not exceed max limit of 3"
        );
    }
}
