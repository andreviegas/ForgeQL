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
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

/// A single row in a query result set.
///
/// Flat row model: every query produces a uniform `SymbolMatch` populated
/// with the fields that make sense for that operation.  Dynamic per-type
/// metadata (signature, value, enum members, etc.) lives in `fields`.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// 2. Populate it in [`SymbolRow::from_match`].
///
/// No per-formatter changes needed unless the formatter has format-specific
/// layout choices (e.g. compact groups by `kind`, text uses `via` prefix).
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
}

impl SymbolRow {
    pub(crate) fn from_match(row: &SymbolMatch) -> Self {
        Self {
            name: compact_name(&row.name).into_owned(),
            kind: row
                .fql_kind
                .as_deref()
                .or(row.node_kind.as_deref())
                .unwrap_or("")
                .to_string(),
            path: row
                .path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            line: row.line.unwrap_or(0),
            enclosing_fn: row.fields.get("enclosing_fn").cloned(),
        }
    }
}

// -----------------------------------------------------------------------
// Show results
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
}

/// An entry in a SHOW outline result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutlineEntry {
    pub name: String,
    pub fql_kind: String,
    pub path: PathBuf,
    pub line: usize,
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
    /// Unified diff (populated for dry-run and explain modes).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Advisory notes (e.g. string literals containing the symbol name).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<SuggestionEntry>,
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
pub(crate) fn compact_name(name: &str) -> std::borrow::Cow<'_, str> {
    if name.contains('\n') {
        std::borrow::Cow::Owned(format!("len:{}", name.len()))
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
                    | ShowContent::Members { .. } => {}
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
            Self::BeginTransaction(_)
            | Self::Commit(_)
            | Self::SourceOp(_)
            | Self::VerifyBuild(_)
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

// -----------------------------------------------------------------------
// Display formatting (human-readable output)
// -----------------------------------------------------------------------

impl fmt::Display for ForgeQLResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(result) => write!(formatter, "{result}"),
            Self::Show(result) => write!(formatter, "{result}"),
            Self::Mutation(result) => write!(formatter, "{result}"),
            Self::SourceOp(result) => write!(formatter, "{result}"),
            Self::BeginTransaction(result) => write!(formatter, "{result}"),
            Self::Commit(result) => write!(formatter, "{result}"),
            Self::Plan(result) => write!(formatter, "{result}"),
            Self::Rollback(result) => write!(formatter, "{result}"),
            Self::VerifyBuild(result) => write!(formatter, "{result}"),
        }
    }
}

impl fmt::Display for QueryResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.results.is_empty() {
            return writeln!(formatter, "No results.");
        }
        for row in &self.results {
            let sr = SymbolRow::from_match(row);
            write!(formatter, "{}", sr.name)?;
            if !sr.kind.is_empty() {
                write!(formatter, " | {}", sr.kind)?;
            }
            if !sr.path.is_empty() {
                write!(formatter, " | {}:{}", sr.path, sr.line)?;
            }
            if let Some(ref fn_name) = sr.enclosing_fn {
                write!(formatter, " | via {fn_name}")?;
            }
            if let Some(usages) = row.usages_count {
                write!(formatter, " | usages: {usages}")?;
            }
            if let Some(count) = row.count {
                write!(formatter, " | count: {count}")?;
            }
            if let Some(value) = row.fields.get("value") {
                write!(formatter, " = {value}")?;
            }
            writeln!(formatter)?;
        }
        if self.total > self.results.len() {
            writeln!(
                formatter,
                "({} of {} shown)",
                self.results.len(),
                self.total,
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for ShowResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref symbol) = self.symbol {
            writeln!(formatter, "--- {symbol} ---")?;
        }
        if let Some(ref file) = self.file {
            writeln!(formatter, "File: {}", file.display())?;
        }
        if let (Some(start), Some(end)) = (self.start_line, self.end_line) {
            writeln!(formatter, "Lines: {start}\u{2013}{end}")?;
        }
        match &self.content {
            ShowContent::Lines { lines, .. } => {
                for source_line in lines {
                    if let Some(ref marker) = source_line.marker {
                        write!(formatter, "{marker} ")?;
                    }
                    writeln!(formatter, "{:>4} | {}", source_line.line, source_line.text,)?;
                }
            }
            ShowContent::Signature {
                signature, line, ..
            } => {
                writeln!(formatter, "{line:>4} | {signature}")?;
            }
            ShowContent::Outline { entries } => {
                for entry in entries {
                    writeln!(
                        formatter,
                        "{:>4} | {:12} | {}",
                        entry.line, entry.fql_kind, entry.name,
                    )?;
                }
            }
            ShowContent::Members { members, .. } => {
                for member in members {
                    writeln!(
                        formatter,
                        "{:>4} | {:12} | {}",
                        member.line, member.fql_kind, member.text,
                    )?;
                }
            }
            ShowContent::CallGraph { entries, .. } => {
                for entry in entries {
                    write!(formatter, "  {}", entry.name)?;
                    if let Some(ref path) = entry.path {
                        write!(formatter, " ({})", path.display())?;
                    }
                    if let Some(line) = entry.line {
                        write!(formatter, ":{line}")?;
                    }
                    writeln!(formatter)?;
                }
            }
            ShowContent::FileList { files, total } => {
                for entry in files {
                    writeln!(formatter, "  {}", entry.path.display())?;
                }
                writeln!(formatter, "({total} files)")?;
            }
        }
        if let Some(ref hint) = self.hint {
            writeln!(formatter, "{hint}")?;
        }
        Ok(())
    }
}

impl fmt::Display for MutationResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.applied { "Applied" } else { "Planned" };
        writeln!(
            formatter,
            "{status}: {} edit(s) in {} file(s)",
            self.edit_count,
            self.files_changed.len(),
        )?;
        for path in &self.files_changed {
            writeln!(formatter, "  {}", path.display())?;
        }
        if let Some(ref diff) = self.diff {
            writeln!(formatter, "\n{diff}")?;
        }
        for suggestion in &self.suggestions {
            writeln!(
                formatter,
                "  note: {} ({}:{})",
                suggestion.snippet,
                suggestion.path.display(),
                suggestion.byte_offset,
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for SourceOpResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref name) = self.source_name {
            write!(formatter, "Source: {name}")?;
        }
        if let Some(ref session_id) = self.session_id {
            write!(formatter, "  Session: {session_id}")?;
        }
        if let Some(count) = self.symbols_indexed {
            write!(formatter, "  ({count} symbols indexed)")?;
        }
        if self.resumed {
            write!(formatter, "  (resumed)")?;
        }
        writeln!(formatter)?;
        if let Some(ref message) = self.message {
            writeln!(formatter, "{message}")?;
        }
        if !self.branches.is_empty() {
            writeln!(formatter, "Branches:")?;
            for branch in &self.branches {
                writeln!(formatter, "  {branch}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for BeginTransactionResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "Checkpoint '{name}' created (oid: {oid})",
            name = self.name,
            oid = self.checkpoint_oid,
        )
    }
}

impl fmt::Display for CommitResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "Committed: {hash}\nMessage: {msg}",
            hash = self.commit_hash,
            msg = self.message,
        )
    }
}

impl fmt::Display for RollbackResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "Rolled back to checkpoint '{name}' (oid: {oid})",
            name = self.name,
            oid = self.reset_to_oid,
        )
    }
}

impl fmt::Display for VerifyBuildResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.success { "PASSED" } else { "FAILED" };
        writeln!(formatter, "VERIFY build '{}': {status}", self.step)?;
        if !self.output.is_empty() {
            writeln!(formatter, "{}", self.output.trim_end())?;
        }
        Ok(())
    }
}

impl fmt::Display for PlanResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total_edits: usize = self.file_edits.iter().map(|fe| fe.edit_count).sum();
        writeln!(
            formatter,
            "Plan: {total_edits} edit(s) in {} file(s)",
            self.file_edits.len(),
        )?;
        for file_edit in &self.file_edits {
            writeln!(
                formatter,
                "  {} ({} edits)",
                file_edit.path.display(),
                file_edit.edit_count,
            )?;
        }
        if !self.diff.is_empty() {
            writeln!(formatter, "\n{diff}", diff = self.diff)?;
        }
        Ok(())
    }
}

// -----------------------------------------------------------------------
// Conversion helpers
// -----------------------------------------------------------------------

impl ForgeQLResult {
    /// Serialize to a JSON string for MCP tool responses and pipe output.
    ///
    /// All inner types derive `Serialize`, so serialization cannot fail
    /// under normal conditions.
    #[must_use]
    pub fn to_json(&self) -> String {
        // Safety: all fields are Serialize-derived primitives, Strings,
        // PathBufs, and Vecs — serialization is infallible.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Serialize to a pretty-printed JSON string.
    ///
    /// All inner types derive `Serialize`, so serialization cannot fail
    /// under normal conditions.
    #[must_use]
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Serialize query results to a compact CSV-style envelope.
    ///
    /// Produces a minimal JSON object with a `results` array where each entry
    /// is a flat string array `["name", "node_kind", "path", "count"]`.
    ///
    /// The `count` column covers two cases:
    /// - `usages_count` — number of references to the symbol (FIND queries)
    /// - `count` — per-file hit count (COUNT … GROUP BY file)
    ///
    /// This format is ~60% smaller than the full JSON and keeps large query
    /// responses within the MCP inline-response threshold.
    ///
    /// Non-query results (mutations, show, source ops) fall back to `to_json()`
    /// since they lack a uniform tabular structure.
    #[must_use]
    pub fn to_csv(&self) -> String {
        let Self::Query(query) = self else {
            return self.to_json();
        };
        // For FIND usages (no GROUP BY) there is no count — each row is one
        // call site and the 4th column carries the line number. Label it
        // honestly so callers are not confused by a "count" that is actually
        // a line number.
        let count_col = if query.op == "find_usages" {
            "line"
        } else {
            "count"
        };
        let mut all_rows: Vec<serde_json::Value> =
            vec![serde_json::json!(["name", "node_kind", "path", count_col])];
        all_rows.extend(query.results.iter().map(|row| {
            // `usages_count` is populated by FIND queries;
            // `count` is populated by COUNT … GROUP BY;
            // `line` is the fallback for FIND usages rows (makes each
            // call site distinguishable when multiple appear in the same file).
            let count_str = row
                .usages_count
                .or(row.count)
                .map(|n| n.to_string())
                .or_else(|| row.line.map(|l| l.to_string()))
                .unwrap_or_default();
            serde_json::json!([
                compact_name(&row.name),
                row.node_kind.as_deref().unwrap_or(""),
                row.path
                    .as_ref()
                    .map(|path| path.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                count_str,
            ])
        }));
        serde_json::to_string(&serde_json::json!({
            "total": query.total,
            "results": all_rows,
        }))
        .unwrap_or_default()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_result_round_trips_through_json() {
        let result = ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results: vec![SymbolMatch {
                name: "setPeakLevel".to_string(),
                node_kind: Some("Function".to_string()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("src/signal_controller.cpp")),
                line: None,
                usages_count: Some(3),
                fields: HashMap::from([(
                    "signature".to_string(),
                    "void setPeakLevel(int level)".to_string(),
                )]),
                count: None,
            }],
            total: 1,
            metric_hint: None,
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::Query(query_result) => {
                assert_eq!(query_result.op, "find_symbols");
                assert_eq!(query_result.results.len(), 1);
                assert_eq!(query_result.results[0].name, "setPeakLevel");
                assert_eq!(query_result.total, 1);
            }
            other => panic!("expected Query variant, got: {other:?}"),
        }
    }

    #[test]
    fn csv_find_result_has_header_and_usages_count() {
        // FIND query: usages_count is populated, count is None.
        let result = ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results: vec![SymbolMatch {
                name: "setPeakLevel".to_string(),
                node_kind: Some("Function".to_string()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("src/signal.cpp")),
                line: None,
                usages_count: Some(7),
                fields: HashMap::new(),
                count: None,
            }],
            total: 1,
            metric_hint: None,
        });
        let csv = result.to_csv();
        let v: serde_json::Value = serde_json::from_str(&csv).unwrap();
        // First element of results is the header row.
        assert_eq!(
            v["results"][0],
            serde_json::json!(["name", "node_kind", "path", "count"])
        );
        // Data row has the usages_count in the 4th column.
        assert_eq!(v["results"][1][0], "setPeakLevel");
        assert_eq!(v["results"][1][3], "7");
        assert_eq!(v["total"], 1);
    }

    #[test]
    fn csv_find_usages_header_says_line_not_count() {
        // FIND usages without GROUP BY: each row is a call site; the 4th
        // column is the line number, so the header must say "line".
        let result = ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results: vec![SymbolMatch {
                name: "processData".to_string(),
                node_kind: Some("identifier".to_string()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("src/main.cpp")),
                line: Some(42),
                usages_count: None,
                fields: HashMap::new(),
                count: None,
            }],
            total: 1,
            metric_hint: None,
        });
        let csv = result.to_csv();
        let v: serde_json::Value = serde_json::from_str(&csv).unwrap();
        assert_eq!(
            v["results"][0],
            serde_json::json!(["name", "node_kind", "path", "line"]),
            "header must say 'line' for find_usages op"
        );
        assert_eq!(
            v["results"][1][3], "42",
            "line number must appear in column 4"
        );
    }

    #[test]
    fn csv_count_group_by_uses_count_field() {
        // COUNT … GROUP BY file: count is populated, usages_count is None.
        let result = ForgeQLResult::Query(QueryResult {
            op: "count_usages".to_string(),
            results: vec![SymbolMatch {
                name: "src/signal.cpp".to_string(),
                node_kind: None,
                fql_kind: None,
                language: None,
                path: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: Some(4),
            }],
            total: 1,
            metric_hint: None,
        });
        let csv = result.to_csv();
        let v: serde_json::Value = serde_json::from_str(&csv).unwrap();
        // Header row present.
        assert_eq!(
            v["results"][0],
            serde_json::json!(["name", "node_kind", "path", "count"])
        );
        // Data row: count field (not usages_count) must appear in column 4.
        assert_eq!(
            v["results"][1][3], "4",
            "count field must map to csv column 4: {csv}"
        );
    }

    #[test]
    fn csv_non_query_result_falls_back_to_json() {
        let result = ForgeQLResult::Mutation(MutationResult {
            op: "rename_symbol".to_string(),
            applied: true,
            files_changed: vec![],
            edit_count: 0,
            lines_written: 0,
            diff: None,
            suggestions: vec![],
        });
        let output = result.to_csv();
        // Must fall back to full JSON, not crash or return empty.
        assert!(output.contains("rename_symbol"), "fallback JSON: {output}");
        assert!(output.contains("applied"), "fallback JSON: {output}");
    }
    #[test]
    fn show_result_round_trips_through_json() {
        let result = ForgeQLResult::Show(ShowResult {
            op: "show_body".to_string(),
            symbol: Some("convertByte2Volts".to_string()),
            file: Some(PathBuf::from("src/adc.cpp")),
            start_line: Some(42),
            end_line: Some(44),
            total_lines: None,
            hint: None,
            content: ShowContent::Lines {
                lines: vec![
                    SourceLine {
                        line: 42,
                        text: "float convertByte2Volts(uint8_t raw) {".to_string(),
                        marker: None,
                    },
                    SourceLine {
                        line: 43,
                        text: "    return raw * 3.3f / 255.0f;".to_string(),
                        marker: None,
                    },
                ],
                byte_start: Some(1024),
                depth: Some(1),
            },
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::Show(show_result) => {
                assert_eq!(show_result.op, "show_body");
                assert_eq!(show_result.symbol.as_deref(), Some("convertByte2Volts"),);
                // Phase 4: start_line and end_line must round-trip.
                assert_eq!(
                    show_result.start_line,
                    Some(42),
                    "start_line must round-trip"
                );
                assert_eq!(show_result.end_line, Some(44), "end_line must round-trip");
            }
            other => panic!("expected Show variant, got: {other:?}"),
        }
    }

    #[test]
    fn mutation_result_round_trips_through_json() {
        let result = ForgeQLResult::Mutation(MutationResult {
            op: "rename_symbol".to_string(),
            applied: true,
            files_changed: vec![
                PathBuf::from("src/signal_controller.cpp"),
                PathBuf::from("include/signal_controller.hpp"),
            ],
            edit_count: 5,
            lines_written: 0,
            diff: None,
            suggestions: vec![SuggestionEntry {
                path: PathBuf::from("src/signal_controller.cpp"),
                byte_offset: 2048,
                snippet: r#"[[deprecated("Use setMaxIntensity()")]]"#.to_string(),
                reason: "deprecated_attribute".to_string(),
            }],
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::Mutation(mutation_result) => {
                assert!(mutation_result.applied);
                assert_eq!(mutation_result.files_changed.len(), 2);
                assert_eq!(mutation_result.suggestions.len(), 1);
            }
            other => panic!("expected Mutation variant, got: {other:?}"),
        }
    }

    #[test]
    fn source_op_result_round_trips_through_json() {
        let result = ForgeQLResult::SourceOp(SourceOpResult {
            op: "use_source".to_string(),
            source_name: Some("pisco-code".to_string()),
            session_id: Some("s1710502345678".to_string()),
            branches: vec!["main".to_string(), "develop".to_string()],
            symbols_indexed: Some(668),
            resumed: false,
            message: None,
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::SourceOp(source_result) => {
                assert_eq!(source_result.op, "use_source");
                assert_eq!(source_result.symbols_indexed, Some(668));
                assert!(!source_result.resumed);
            }
            other => panic!("expected SourceOp variant, got: {other:?}"),
        }
    }

    #[test]
    fn begin_transaction_result_round_trips_through_json() {
        let result = ForgeQLResult::BeginTransaction(BeginTransactionResult {
            name: "rename-signal-api".to_string(),
            checkpoint_oid: "abc123def456".to_string(),
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::BeginTransaction(bt) => {
                assert_eq!(bt.name, "rename-signal-api");
                assert_eq!(bt.checkpoint_oid, "abc123def456");
            }
            other => panic!("expected BeginTransaction variant, got: {other:?}"),
        }
    }

    #[test]
    fn commit_result_round_trips_through_json() {
        let result = ForgeQLResult::Commit(CommitResult {
            message: "Rename signal controller API".to_string(),
            commit_hash: "abc123def456".to_string(),
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::Commit(c) => {
                assert_eq!(c.message, "Rename signal controller API");
                assert_eq!(c.commit_hash, "abc123def456");
            }
            other => panic!("expected Commit variant, got: {other:?}"),
        }
    }

    #[test]
    fn plan_result_round_trips_through_json() {
        let result = ForgeQLResult::Plan(PlanResult {
            op: "dry_run".to_string(),
            diff: "--- a/src/signal.cpp\n+++ b/src/signal.cpp\n".to_string(),
            file_edits: vec![FileEditSummary {
                path: PathBuf::from("src/signal.cpp"),
                edit_count: 3,
            }],
            suggestions: vec![],
        });

        let json_string = result.to_json();
        let deserialized: ForgeQLResult = serde_json::from_str(&json_string).unwrap();

        match deserialized {
            ForgeQLResult::Plan(plan_result) => {
                assert_eq!(plan_result.op, "dry_run");
                assert_eq!(plan_result.file_edits.len(), 1);
                assert_eq!(plan_result.file_edits[0].edit_count, 3);
            }
            other => panic!("expected Plan variant, got: {other:?}"),
        }
    }

    #[test]
    fn display_query_result_empty() {
        let result = QueryResult {
            op: "find_symbols".to_string(),
            results: vec![],
            total: 0,
            metric_hint: None,
        };
        let output = format!("{result}");
        assert!(output.contains("No results"));
    }

    #[test]
    fn display_query_result_with_items() {
        let result = QueryResult {
            op: "find_symbols".to_string(),
            results: vec![SymbolMatch {
                name: "setPeakLevel".to_string(),
                node_kind: Some("Function".to_string()),
                fql_kind: None,
                language: None,
                path: Some(PathBuf::from("src/signal.cpp")),
                line: Some(42),
                usages_count: Some(3),
                fields: HashMap::new(),
                count: None,
            }],
            total: 1,
            metric_hint: None,
        };
        let output = format!("{result}");
        assert!(output.contains("setPeakLevel"));
        assert!(output.contains("Function"));
        assert!(output.contains("src/signal.cpp:42"));
        assert!(output.contains("usages: 3"));
    }

    #[test]
    fn display_query_result_shows_enclosing_fn() {
        let mut fields = HashMap::new();
        fields.insert("enclosing_fn".to_string(), "traverse_trees".to_string());
        let result = QueryResult {
            op: "find_symbols".to_string(),
            results: vec![SymbolMatch {
                name: "(a&&(b||c))".to_string(),
                node_kind: None,
                fql_kind: Some("if".to_string()),
                language: None,
                path: Some(PathBuf::from("tree-walk.c")),
                line: Some(899),
                usages_count: Some(0),
                fields,
                count: None,
            }],
            total: 1,
            metric_hint: None,
        };
        let output = format!("{result}");
        assert!(output.contains("via traverse_trees"));
        assert!(output.contains("tree-walk.c:899"));
    }

    #[test]
    fn display_query_result_shows_truncation_notice() {
        let result = QueryResult {
            op: "find_symbols".to_string(),
            results: vec![SymbolMatch {
                name: "foo".to_string(),
                node_kind: None,
                fql_kind: None,
                language: None,
                path: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: None,
            }],
            total: 100,
            metric_hint: None,
        };
        let output = format!("{result}");
        assert!(output.contains("1 of 100 shown"));
    }

    #[test]
    fn display_mutation_result_applied() {
        let result = MutationResult {
            op: "rename_symbol".to_string(),
            applied: true,
            files_changed: vec![PathBuf::from("src/main.cpp")],
            edit_count: 4,
            lines_written: 0,
            diff: None,
            suggestions: vec![],
        };
        let output = format!("{result}");
        assert!(output.contains("Applied"));
        assert!(output.contains("4 edit(s)"));
        assert!(output.contains("1 file(s)"));
    }

    #[test]
    fn display_plan_result() {
        let result = PlanResult {
            op: "dry_run".to_string(),
            diff: "--- a/test.cpp\n+++ b/test.cpp\n@@ -1 +1 @@\n-old\n+new\n".to_string(),
            file_edits: vec![FileEditSummary {
                path: PathBuf::from("test.cpp"),
                edit_count: 1,
            }],
            suggestions: vec![],
        };
        let output = format!("{result}");
        assert!(output.contains("1 edit(s)"));
        assert!(output.contains("test.cpp"));
        assert!(output.contains("-old"));
        assert!(output.contains("+new"));
    }
}
