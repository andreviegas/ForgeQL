//! Result types for read-only queries (FIND, COUNT) and their projected rows.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{compact_name, surface_block_alias};

/// Result of FIND NODE id — resolved node details and navigation links.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindNodeResult {
    pub node_id: String,
    pub fql_kind: String,
    pub name: String,
    pub path: PathBuf,
    pub line: usize,
    pub end_line: usize,
    /// SHA-256 of node bytes as h{:016x}; empty for analysis-only rows.
    pub rev: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_child_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_sibling_node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_sibling_node_id: Option<String>,
}

// -----------------------------------------------------------------------
// Query results
// -----------------------------------------------------------------------

/// Result of a read-only query operation (FIND, COUNT).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    /// Operation name: `"find_symbols"`, `"find_usages"`, `"count_usages"`, etc.
    pub op: String,
    /// Matched items.  The `SymbolMatch` struct is flexible enough to represent
    /// symbols, usages, defines, enums, includes, and files.
    pub results: Vec<SymbolMatch>,
    /// Total number of results (before LIMIT truncation, if applicable).
    pub total: usize,
    /// When the query has a numeric WHERE on an enrichment field (e.g.
    /// `member_count > 10`), the compact renderer shows that field's value
    /// as the last column instead of the default `usages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric_hint: Option<String>,
    /// When a GROUP BY targets a field other than `fql_kind` (e.g. `file` or
    /// `guard_kind`), this stores the field name so the compact renderer labels
    /// the outer column with it and groups by its value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_field: Option<String>,
    /// One-line guidance appended by the engine when the query shape is a
    /// known footgun (e.g. a WHERE field that no row type carries — the
    /// query silently matches nothing). Static text keyed on the observed
    /// input; never populated on ordinary results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Master rev of the `LAST` set this FIND armed — the handle a bulk
    /// `… NODE[S] LAST` mutation must quote in `IF REV`.
    ///
    /// Absent when the result was truncated (a set the agent has not seen in
    /// full is never gated, so no LAST verb will act on it) or when the rows
    /// carry nothing addressable (a `GROUP BY` aggregate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub found_rev: Option<String>,
}
impl QueryResult {
    /// Build a [`QueryContext`] from query-level metadata.
    pub(crate) fn query_context(&self) -> QueryContext<'_> {
        QueryContext {
            metric_hint: self.metric_hint.as_deref(),
            group_by_field: self.group_by_field.as_deref(),
        }
    }

    /// Project all result rows into display-ready [`SymbolRow`] values.
    ///
    /// This is the **single entry point** for all output formatters.
    /// No formatter should access `self.results` directly for display.
    pub(crate) fn projected_rows(&self) -> Vec<SymbolRow> {
        let ctx = self.query_context();
        self.results
            .iter()
            .map(|r| SymbolRow::from_match_with_ctx(r, &ctx))
            .collect()
    }
}
/// A single row in a query result set.
///
/// Flat row model: every query produces a uniform `SymbolMatch` populated
/// with the fields that make sense for that operation.  Dynamic per-type
/// metadata (signature, value, enum members, etc.) lives in `fields`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SymbolMatch {
    /// Symbol, macro, enum, or file name.
    pub name: String,
    /// AST node kind: `"function_definition"`, `"declaration"`, `"identifier"`, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_kind: Option<String>,
    /// Universal FQL kind (e.g. `"function"`, `"class"`, `"number"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fql_kind: Option<String>,
    /// Language identifier (e.g. `"cpp"`, `"typescript"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Source file path (relative to workspace root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// 1-based line number of the symbol's definition or usage site.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Number of times this symbol is referenced across the codebase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usages_count: Option<usize>,
    /// Dynamic metadata fields from the index row.
    ///
    /// Examples: `"signature"`, `"value"` (for `#define`), `"type"`, `"body"`.
    /// Populated only when the underlying index row carries that data.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub fields: HashMap<String, String>,
    /// Per-file usage count (for COUNT USAGES ... GROUP BY file).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// Stable node handle; `None` for analysis-only rows (numbers, operators, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Content rev of the node the handle addresses — what `IF REV` takes.
    ///
    /// Always handed out **with** the handle, never separately. A handle without
    /// its rev is an address an agent cannot safely act on, and making it fetch
    /// the rev costs a round trip per edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
}
// -----------------------------------------------------------------------
// Query context — carries query-level metadata for row projection
// -----------------------------------------------------------------------

/// Context extracted from a [`QueryResult`] that controls which fields
/// [`SymbolRow::from_match_with_ctx`] populates.
///
/// Formatters never see `SymbolMatch` directly — they receive projected
/// `SymbolRow` values built using this context.
// pub(crate), not pub: re-exported at crate scope via `pub use query::*` in the
// parent module, where the pub(crate)-vs-pub distinction is meaningful.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct QueryContext<'a> {
    /// When the query uses a numeric WHERE/ORDER BY on an enrichment field
    /// (e.g. `lines`, `param_count`), this names that field so its value
    /// appears as the metric column instead of `usages`.
    pub metric_hint: Option<&'a str>,
    /// When GROUP BY targets a field other than `fql_kind`, this names that
    /// field so its value is extracted into `SymbolRow::group_key`.
    pub group_by_field: Option<&'a str>,
}

// -----------------------------------------------------------------------
// Per-row display data — consumed by all output formatters
// -----------------------------------------------------------------------

/// Unified per-row display data extracted from a [`SymbolMatch`].
///
/// All output formatters — text (`Display`), compact, and JSON — derive their
/// row representation from this struct.  Adding a new display column requires
/// only two changes:
///
/// 1. Add the field here.
/// 2. Populate it in [`SymbolRow::from_match_with_ctx`].
///
/// No per-formatter changes needed unless the formatter has format-specific
/// layout choices (e.g. compact groups by `kind`, text uses `via` prefix).
#[allow(dead_code)]
// Fields used in upcoming formatter migration (Phases 2–4).
// pub(crate), not pub: re-exported at crate scope via `pub use query::*` in the
// parent module, where the pub(crate)-vs-pub distinction is meaningful.
#[allow(clippy::redundant_pub_crate)]
pub(crate) struct SymbolRow {
    /// Compact display name (long names are truncated to 120 chars).
    pub name: String,
    /// FQL kind (`"function"`, `"if"`, …) or raw AST node kind as fallback.
    pub kind: String,
    /// File path (relative), empty string when absent.
    pub path: String,
    /// 1-based line number; 0 when absent.
    pub line: usize,
    /// Enclosing function name — populated for control-flow nodes (if/switch/for/while).
    /// `None` for top-level symbols (functions, structs, etc.).
    pub enclosing_fn: Option<String>,
    /// Number of times this symbol is referenced across the codebase.
    pub usages: Option<usize>,
    /// Per-group count — populated after GROUP BY aggregation.
    pub count: Option<usize>,
    /// Value of the enrichment field named by `QueryContext::metric_hint`.
    /// Shown as the last column instead of `usages` when present.
    pub metric_value: Option<String>,
    /// Value of the custom GROUP BY field (e.g. `guard_kind = "preprocessor"`).
    /// Used as the row label/group key when GROUP BY targets an enrichment field.
    pub group_key: Option<String>,
    /// Stable node handle computed from the file path and per-file DFS ordinal.
    /// Format: `n{12-hex segment_id}.{ordinal:04}`.
    /// `None` for rows from legacy segments that have not been reindexed.
    pub node_id: Option<String>,
    /// Content rev of that node — rendered next to the handle, never apart from
    /// it, because a handle you cannot gate is a handle you cannot safely use.
    pub rev: Option<String>,
}

impl SymbolRow {
    /// Build a display row from a query match, using query-level context to
    /// decide which enrichment fields to extract.
    pub(crate) fn from_match_with_ctx(row: &SymbolMatch, ctx: &QueryContext<'_>) -> Self {
        Self {
            name: compact_name(&row.name).into_owned(),
            // `node_kind` is deprecated and intentionally NOT used as a
            // fallback — only `fql_kind` is exposed.  Backends that lack a
            // mapped `fql_kind` for a given AST node return an empty string,
            // matching the columnar backend's behaviour for parity.
            kind: row.fql_kind.clone().unwrap_or_default(),
            path: row
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            line: row.line.unwrap_or(0),
            enclosing_fn: row.fields.get("enclosing_fn").cloned(),
            usages: row.usages_count,
            count: row.count,
            metric_value: ctx
                .metric_hint
                .and_then(|field| row.fields.get(field).cloned()),
            group_key: ctx.group_by_field.and_then(|field| match field {
                "language" | "lang" => row.language.clone(),
                "node_kind" => row.node_kind.clone(),
                "fql_kind" => row.fql_kind.clone(),
                "name" => Some(row.name.clone()),
                "file" | "path" => row.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                _ => row.fields.get(field).cloned(),
            }),
            node_id: surface_block_alias(row),
            rev: row.rev.clone(),
        }
    }

    /// The display string for the last column in CSV output.
    ///
    /// Unlike `metric()`, this preserves non-numeric enrichment strings
    /// (e.g. `cast_style = "c_style"`) instead of falling back to 0.
    ///
    /// Priority: `count` (GROUP BY) → `metric_value` (enrichment, verbatim) → `usages`.
    pub(crate) fn metric_str(&self) -> String {
        if let Some(c) = self.count {
            return c.to_string();
        }
        if let Some(ref v) = self.metric_value {
            return v.clone();
        }
        self.usages.unwrap_or(0).to_string()
    }
}
