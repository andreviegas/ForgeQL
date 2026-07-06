/// Typed result types for every `ForgeQL` operation.
///
/// These replace all `serde_json::Value` returns from the executor.  Transport
/// layers (MCP, REPL, pipe) serialize or format these as needed — the core
/// library never decides the wire format.
///
/// # Design
///
/// - Every operation returns a `ForgeQLResult` variant.
/// - Inner structs are `Serialize + Deserialize` for MCP JSON transport.
/// - `ForgeQLResult::to_display()` produces human-friendly terminal output.
/// - No `serde_json::Value` appears anywhere in this module.
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod convert;
mod display;

// -----------------------------------------------------------------------
// Top-level result enum
// -----------------------------------------------------------------------

/// The unified return type for all `ForgeQL` operations.
///
/// The engine's `execute()` method returns this; transport layers convert it
/// to JSON (MCP) or formatted text (REPL/pipe) without re-interpreting the
/// inner data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgeQLResult {
    /// Read-only queries: FIND symbols, FIND usages, FIND defines, etc.
    Query(QueryResult),
    /// Code exposure: SHOW body, SHOW outline, SHOW members, etc.
    Show(ShowResult),
    /// Mutations: RENAME, CHANGE, MIGRATE DEFINE, MIGRATE ENUM, etc.
    Mutation(MutationResult),
    /// Source and session lifecycle: CREATE SOURCE, USE, DISCONNECT, etc.
    SourceOp(SourceOpResult),
    /// Checkpoint: BEGIN TRANSACTION 'name'
    BeginTransaction(BeginTransactionResult),
    /// Commit: COMMIT MESSAGE 'msg'
    Commit(CommitResult),
    /// Plan preview: `DRY_RUN` and `EXPLAIN` (never writes files).
    Plan(PlanResult),
    /// Rollback: ROLLBACK [TRANSACTION 'name']
    Rollback(RollbackResult),
    /// Standalone verify: VERIFY build 'step'
    VerifyBuild(VerifyBuildResult),
    /// Output of a standalone `RUN '<step>' <args…>` command template.
    Run(RunResult),
    /// Node-addressed lookup: FIND NODE id
    FindNode(FindNodeResult),
    /// Background job submitted: `JOB START '<label>'`
    JobStarted(JobStartedResult),
    /// Background job status: `JOB STATUS '<id>'`
    JobStatus(crate::jobs::JobSnapshot),
    /// Background job list: `JOB LIST`
    JobList(JobListResult),
}

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
    /// When a GROUP BY on a custom field is used (e.g. `guard_kind`), this
    /// stores the field name so the compact renderer can group by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by_field: Option<String>,
    /// One-line guidance appended by the engine when the query shape is a
    /// known footgun (e.g. a WHERE field that no row type carries — the
    /// query silently matches nothing). Static text keyed on the observed
    /// input; never populated on ordinary results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
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
}
// -----------------------------------------------------------------------
// Query context — carries query-level metadata for row projection
// -----------------------------------------------------------------------

/// Context extracted from a [`QueryResult`] that controls which fields
/// [`SymbolRow::from_match_with_ctx`] populates.
///
/// Formatters never see `SymbolMatch` directly — they receive projected
/// `SymbolRow` values built using this context.
pub(crate) struct QueryContext<'a> {
    /// When the query uses a numeric WHERE/ORDER BY on an enrichment field
    /// (e.g. `lines`, `param_count`), this names that field so its value
    /// appears as the metric column instead of `usages`.
    pub metric_hint: Option<&'a str>,
    /// When GROUP BY targets a custom field (not `fql_kind`/`file`), this
    /// names that field so its value is extracted into `SymbolRow::group_key`.
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
#[allow(dead_code)] // Fields used in upcoming formatter migration (Phases 2–4).
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
}

/// Stage 2 block alias: when a row is a block member (it carries `block_ord` /
/// `block_off` fields written at index time), surface its handle as
/// `block_id(offset)` instead of the member's own node id. The member's segment
/// prefix is reused, so only the ordinal and offset change. Members still
/// resolve by their own node id; this only changes what FIND/SHOW display.
fn surface_block_alias(row: &SymbolMatch) -> Option<String> {
    let own = row.node_id.as_deref()?;
    Some(crate::node_id::surface_block_id(
        own,
        row.fields.get("block_ord").map(String::as_str),
        row.fields.get("block_off").map(String::as_str),
    ))
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
                "path" => row.path.as_ref().map(|p| p.to_string_lossy().into_owned()),
                _ => row.fields.get(field).cloned(),
            }),
            node_id: surface_block_alias(row),
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

// -----------------------------------------------------------------------
// Show results
// -----------------------------------------------------------------------

/// Result of a code exposure (SHOW) operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowResult {
    /// Operation name: `"show_body"`, `"show_outline"`, `"show_members"`, etc.
    pub op: String,
    /// The symbol or file this result describes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Source file path (relative to workspace root).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    /// The rendered code/text content.
    pub content: ShowContent,
    /// First 1-based line of the shown entity's full span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    /// Last 1-based line of the shown entity's full span (inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    /// Total number of source lines before truncation (set only when the
    /// implicit line cap fires).  Absent when all lines are returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_lines: Option<usize>,
    /// Guidance message when the output was truncated by the implicit line
    /// cap, telling the agent how to see the remaining lines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Enrichment metadata for DEPTH 0 results (lines, `param_count`, etc.).
    /// Present only when SHOW body returns a signature-only view so the agent
    /// can decide whether to request deeper expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
}

/// The payload of a SHOW result — either structured lines or a list of members.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShowContent {
    /// Line-oriented content: SHOW body, SHOW lines, SHOW context.
    Lines {
        lines: Vec<SourceLine>,
        #[serde(skip_serializing_if = "Option::is_none")]
        byte_start: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        depth: Option<usize>,
    },
    /// Signature-only content: SHOW signature.
    Signature {
        signature: String,
        line: usize,
        byte_start: usize,
    },
    /// Structured outline: SHOW outline (list of declarations in a file).
    Outline { entries: Vec<OutlineEntry> },
    /// Class/struct/enum members: SHOW members.
    Members {
        members: Vec<MemberEntry>,
        byte_start: usize,
    },
    /// Call graph results: SHOW callers, SHOW callees.
    CallGraph {
        direction: CallDirection,
        entries: Vec<CallGraphEntry>,
    },
    /// File listing results: FIND files.
    FileList { files: Vec<FileEntry>, total: usize },
    /// Internal stats: SHOW STATS.
    Stats { sessions: Vec<SessionStats> },
}

/// A single source line in a SHOW result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceLine {
    /// 1-based line number in the source file.
    pub line: usize,
    /// The text content of the line.
    pub text: String,
    /// Optional marker for context display (e.g. `">>>"` for the target line).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
    /// Stable node handle for lines that start an addressable node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// 1-based offset of this line within its innermost containing node, when
    /// `SHOW LINES` resolved one. Lets the agent target the line with
    /// `CHANGE NODE 'id(offset)'` instead of an absolute, drift-prone number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_offset: Option<usize>,
}

/// An entry in a SHOW outline result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlineEntry {
    pub name: String,
    pub fql_kind: String,
    pub path: PathBuf,
    pub line: usize,
    /// Stable node handle — `None` for entries from legacy segments without reindex.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Nesting depth in the structural tree (0 = top-level declaration). Lets
    /// the renderer indent children under their enclosing declaration.
    #[serde(default)]
    pub depth: usize,
}

/// An entry in a SHOW members result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberEntry {
    /// Member kind: `"field"`, `"method"`, `"enumerator"`.
    pub fql_kind: String,
    /// Declaration text (trimmed).
    pub text: String,
    /// 1-based line number.
    pub line: usize,
}

/// Direction of a call graph query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    /// SHOW callers — incoming edges (who calls this symbol).
    Callers,
    /// SHOW callees — outgoing edges (what this symbol calls).
    Callees,
}

/// An entry in a call graph result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallGraphEntry {
    /// Function or symbol name.
    pub name: String,
    /// Source file path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// 1-based line number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Byte offset of the call site.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_start: Option<usize>,
}

/// A file entry in a FIND files result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    /// File extension without the leading `.` (e.g. `"cpp"`, `"h"`, `""`
    /// for extension-less files).
    #[serde(default)]
    pub extension: String,
    /// File size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Per-group file count populated after `GROUP BY` aggregation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
}

impl FileEntry {
    /// True for session infrastructure that should never surface in a
    /// `FIND files` listing: the worktree gitfile pointer (`.git`) and
    /// forgeql's own runtime artifacts (`.forgeql-session`,
    /// `.forgeql-index`, …). `COMMIT` already excludes the same set.
    #[must_use]
    pub fn is_runtime_artifact(path: &std::path::Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == ".git" || n.starts_with(".forgeql-"))
    }
}

/// Internal stats for one loaded session, produced by `SHOW STATS`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStats {
    /// Session alias (the identifier passed to `USE … AS`).
    pub session_id: String,
    /// Source name (repo) bound to this session.
    pub source: String,
    /// Branch name bound to this session.
    pub branch: String,
    /// Total number of indexed rows (symbols).
    pub rows: usize,
    /// Distinct symbol names in the intern pool.
    pub distinct_names: usize,
    /// Distinct file paths in the intern pool.
    pub distinct_paths: usize,
    /// Number of distinct symbols that have at least one usage site.
    pub usage_symbols: usize,
    /// Total number of individual usage-site records.
    pub usage_sites: usize,
    /// Number of distinct trigrams in the trigram index.
    pub trigram_distinct: usize,
    /// Approximate heap bytes consumed by the index (all components).
    pub mem_total_bytes: usize,
    /// Approximate heap bytes: `rows` `Vec` + per-row enrichment `HashMap`s.
    pub mem_rows_bytes: usize,
    /// Approximate heap bytes: usages `HashMap`.
    pub mem_usages_bytes: usize,
    /// Approximate heap bytes: `name/kind/fql_kind` secondary indexes.
    pub mem_indexes_bytes: usize,
    /// Approximate heap bytes: trigram index posting lists.
    pub mem_trigram_bytes: usize,
    /// Approximate heap bytes: all five intern pools (`ColumnarTable`).
    pub mem_strings_bytes: usize,
    /// By-language symbol counts (from `IndexStats`).
    pub by_language: std::collections::HashMap<String, usize>,
    /// By-fql_kind symbol counts (from `IndexStats`).
    pub by_fql_kind: std::collections::HashMap<String, usize>,
}

// -----------------------------------------------------------------------
// Mutation results
// -----------------------------------------------------------------------

/// Result of a mutation operation (RENAME, CHANGE, MIGRATE).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MutationResult {
    /// Operation name: `"rename_symbol"`, `"change_content"`, etc.
    pub op: String,
    /// Whether the changes were written to disk.
    pub applied: bool,
    /// Files that were (or would be) modified.
    pub files_changed: Vec<PathBuf>,
    /// Total number of individual byte-range edits.
    pub edit_count: usize,
    /// Total number of lines in all replacement texts.
    ///
    /// Used by the budget system: the agent earns proportional recovery
    /// based on how many lines it actually wrote.
    pub lines_written: usize,
    /// Total number of lines in the original spans replaced or deleted by this
    /// mutation. Paired with `lines_written`, it is the loudest mechanical
    /// signal of a destructive edit: replacing a 60-line node with a 6-line
    /// body reports `lines_removed: 54, lines_written: 6`. The engine stays
    /// mechanical — it reports the line arithmetic and leaves the judgement to
    /// the agent.
    #[serde(default)]
    pub lines_removed: usize,
    /// Unified diff (populated for dry-run and explain modes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Advisory notes (e.g. string literals containing the symbol name)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<SuggestionEntry>,
    /// New node ID of the mutated/inserted symbol after reindex.
    ///
    /// * `CHANGE NODE`: same as the input `node_id` (ordinal is stable after
    ///   body replacement); confirmed by a post-reindex `find_node` lookup.
    /// * `INSERT BEFORE|AFTER NODE`: `node_id` of the first symbol found at
    ///   the insertion line after reindex, or `None` if the inserted content
    ///   contained no addressable symbol at that line.
    /// * `DELETE NODE`: always `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_node_id: Option<String>,
}

/// An advisory note about a potential issue found during planning.
///
/// For example, a string literal that contains the renamed symbol name
/// but was intentionally left unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionEntry {
    /// Source file path.
    pub path: PathBuf,
    /// Byte offset in the file.
    pub byte_offset: usize,
    /// Short excerpt of the surrounding code.
    pub snippet: String,
    /// Why this candidate was flagged.
    pub reason: String,
}

// -----------------------------------------------------------------------
// Source/session operation results
// -----------------------------------------------------------------------

/// Result of a source or session lifecycle operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceOpResult {
    /// Operation name: `"create_source"`, `"use_source"`, `"disconnect"`, etc.
    pub op: String,
    /// Source repository name (if applicable).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,
    /// Session identifier (returned by USE, consumed by subsequent commands).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Available branches (returned by CREATE SOURCE, SHOW BRANCHES).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branches: Vec<String>,
    /// Number of symbols indexed (returned by USE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols_indexed: Option<usize>,
    /// Whether an existing session was resumed (vs. created fresh).
    #[serde(default)]
    pub resumed: bool,
    /// Human-readable status message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

// -----------------------------------------------------------------------
// Transaction results
// -----------------------------------------------------------------------

/// Result of a `BEGIN TRANSACTION 'name'` — checkpoint created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeginTransactionResult {
    /// Checkpoint label.
    pub name: String,
    /// Git commit OID recorded as the checkpoint.
    pub checkpoint_oid: String,
}

/// Result of a `COMMIT MESSAGE 'msg'` — git commit created.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitResult {
    /// Commit message.
    pub message: String,
    /// Git commit hash of the new commit.
    pub commit_hash: String,
}

// -----------------------------------------------------------------------
// Rollback result
// -----------------------------------------------------------------------

/// Result of a `ROLLBACK [TRANSACTION 'name']` operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollbackResult {
    /// The checkpoint label (or `"last"` if none was specified).
    pub name: String,
    /// Git commit OID that was reset to.
    pub reset_to_oid: String,
}

// -----------------------------------------------------------------------
// Verify build result
// -----------------------------------------------------------------------

/// Result of a standalone `VERIFY build 'step'` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyBuildResult {
    /// The verify step name that was run.
    pub step: String,
    /// Whether the step command exited successfully.
    pub success: bool,
    /// Combined stdout + stderr output from the command.
    pub output: String,
    /// Number of output lines to show inline before buffering the rest for
    /// `SHOW MORE`. Resolved from the step's `summary` config at run time.
    #[serde(default = "default_summary_lines")]
    pub summary_lines: usize,
    /// Which end of the output to show inline (tail by default).
    #[serde(default)]
    pub summary_direction: crate::config::SummaryDirection,
}

/// Result of `JOB START '<label>'` — the submitted job's id and label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStartedResult {
    /// Opaque job id to poll with `JOB STATUS`.
    pub id: String,
    /// The verify-step label this job runs.
    pub label: String,
}

/// Result of `JOB LIST` — summaries of all known background jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobListResult {
    /// Jobs in submission order (newest last).
    pub jobs: Vec<crate::jobs::JobSummary>,
}

/// Serde default for [`VerifyBuildResult::summary_lines`].
const fn default_summary_lines() -> usize {
    40
}

/// Result of a standalone `RUN '<step>' <args…>` command.
///
/// The output of an allowlisted `run_steps` template. Shape mirrors
/// [`VerifyBuildResult`]; the distinct type lets the renderer label it `RUN`
/// and buffer its output for `SHOW MORE`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    /// The run step (template) name that was executed.
    pub step: String,
    /// Whether the command exited successfully.
    pub success: bool,
    /// Combined stdout + stderr output from the command.
    pub output: String,
    /// Number of output lines to show inline before buffering the rest for
    /// `SHOW MORE`. Resolved from the step's `summary` config at run time.
    #[serde(default = "default_summary_lines")]
    pub summary_lines: usize,
    /// Which end of the output to show inline (tail by default).
    #[serde(default)]
    pub summary_direction: crate::config::SummaryDirection,
}

// -----------------------------------------------------------------------
// Plan results (DRY_RUN, EXPLAIN)
// -----------------------------------------------------------------------
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    /// Operation name: `"dry_run"` or `"explain"`.
    pub op: String,
    /// Unified diff showing what would change.
    pub diff: String,
    /// Summary of edits per file.
    pub file_edits: Vec<FileEditSummary>,
    /// Advisory notes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<SuggestionEntry>,
}

/// Summary of edits planned for one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEditSummary {
    /// Source file path.
    pub path: PathBuf,
    /// Number of byte-range edits in this file.
    pub edit_count: usize,
}

// -----------------------------------------------------------------------
// Display helpers
// -----------------------------------------------------------------------

/// Compact a symbol name for display.  Multi-line names (e.g. block comments)
/// are replaced with `len:<bytes>` so they don't flood the output.
/// Single-line names longer than 120 chars are truncated with `…`.
/// A single-line orientation snippet of a (possibly multi-line) name: the first
/// line, trimmed, truncated to 120 chars, with a trailing `…` when any content
/// was dropped. Used so a comment name never spills raw multi-line text into the
/// name column while still hinting what the comment says.
/// A single-line orientation snippet of a (possibly multi-line) name: the first
/// line that carries real (alphanumeric) content, trimmed, truncated to 120
/// chars, with a trailing `…` when any content was dropped. Bare comment openers
/// like `/**`, `/*`, `//` are skipped so block comments surface their text, not a
/// delimiter. Used so a comment name never spills raw multi-line text into the
/// name column while still hinting what the comment says.
pub(crate) fn comment_snippet(name: &str) -> String {
    let max = 120usize;
    let full = name.trim();
    let chosen = name
        .lines()
        .map(str::trim)
        .find(|l| l.chars().any(char::is_alphanumeric))
        .or_else(|| name.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("");
    let dropped = chosen.len() < full.len();
    let mut snippet: String = chosen.chars().take(max).collect();
    if dropped || chosen.chars().count() > max {
        snippet.push('…');
    }
    snippet
}

pub(crate) fn compact_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.contains('\n') {
        std::borrow::Cow::Owned(comment_snippet(name))
    } else if name.len() > 120 {
        std::borrow::Cow::Owned(format!("{}…", &name[..120]))
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

// -----------------------------------------------------------------------
// Path relativization — strip worktree prefix from all paths
// -----------------------------------------------------------------------

/// Strip `root` from the front of `path`, returning a relative path.
/// Falls back to the original path if it doesn't start with root.
fn relativize(path: &mut PathBuf, root: &Path) {
    if let Ok(rel) = path.strip_prefix(root) {
        *path = rel.to_path_buf();
    }
}

impl ForgeQLResult {
    /// Strip absolute worktree prefixes from all paths in this result.
    ///
    /// Converts `/data/worktrees/s123/src/foo.cpp` → `src/foo.cpp`.
    /// Called by the engine after every `execute()` so that transport layers
    /// (MCP JSON, REPL, pipe) never see internal filesystem paths.
    pub fn relativize_paths(&mut self, worktree_root: &Path) {
        match self {
            Self::Query(q) => {
                for row in &mut q.results {
                    if let Some(ref mut p) = row.path {
                        relativize(p, worktree_root);
                    }
                }
            }
            Self::Show(s) => {
                if let Some(ref mut p) = s.file {
                    relativize(p, worktree_root);
                }
                match &mut s.content {
                    ShowContent::Outline { entries } => {
                        for entry in entries {
                            relativize(&mut entry.path, worktree_root);
                        }
                    }
                    ShowContent::CallGraph { entries, .. } => {
                        for entry in entries {
                            if let Some(ref mut p) = entry.path {
                                relativize(p, worktree_root);
                            }
                        }
                    }
                    ShowContent::FileList { files, .. } => {
                        for entry in files {
                            relativize(&mut entry.path, worktree_root);
                        }
                    }
                    ShowContent::Lines { .. }
                    | ShowContent::Signature { .. }
                    | ShowContent::Members { .. }
                    | ShowContent::Stats { .. } => {}
                }
            }
            Self::Mutation(m) => {
                for p in &mut m.files_changed {
                    relativize(p, worktree_root);
                }
                for s in &mut m.suggestions {
                    relativize(&mut s.path, worktree_root);
                }
            }
            Self::Plan(p) => {
                for fe in &mut p.file_edits {
                    relativize(&mut fe.path, worktree_root);
                }
                for s in &mut p.suggestions {
                    relativize(&mut s.path, worktree_root);
                }
            }
            Self::FindNode(r) => relativize(&mut r.path, worktree_root),
            Self::BeginTransaction(_)
            | Self::JobStarted(_)
            | Self::JobStatus(_)
            | Self::JobList(_)
            | Self::Commit(_)
            | Self::SourceOp(_)
            | Self::VerifyBuild(_)
            | Self::Run(_)
            | Self::Rollback(_) => {}
        }
    }

    /// Count the number of raw source-code lines contained in this result.
    ///
    /// Used by the query logger to track how much source code was disclosed
    /// to the AI agent per operation.  Only `SHOW` results that return actual
    /// file lines contribute (`SHOW LINES`, `SHOW body`, `SHOW context`).
    /// Structured metadata results (outline, members, call graph) and all
    /// query / mutation results return `0` because they carry no raw source
    /// code.
    #[must_use]
    pub const fn source_lines_count(&self) -> usize {
        if let Self::Show(ShowResult {
            content: ShowContent::Lines { lines, .. },
            ..
        }) = self
        {
            lines.len()
        } else {
            0
        }
    }

    /// Inject a hint string into the result.
    ///
    /// For `Show` results, appends to the existing `hint` field.
    /// For other result types, the hint is silently dropped.
    pub fn inject_hint(&mut self, tip: &str) {
        if let Self::Show(show) = self {
            match show.hint {
                Some(ref mut existing) => {
                    existing.push(' ');
                    existing.push_str(tip);
                }
                None => {
                    show.hint = Some(tip.to_string());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
