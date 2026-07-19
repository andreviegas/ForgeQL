//! Result types for code-exposure (SHOW) operations.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::SessionStats;

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
    /// Pre-rendered paging output: SHOW MORE replays lines that were ALREADY
    /// rendered into the continuation buffer. They must be emitted verbatim —
    /// routing them back through the field-quoting CSV writer would double-encode
    /// every field and surface the buffered header row as a bogus data row.
    Paged { lines: Vec<String> },
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
    /// SHA-256 rev of the innermost node named by `node_id`, as `h{:016x}` — the
    /// value a mutation IF REV needs. Lets a SHOW NODE / SHOW LINES read feed a
    /// CHANGE NODE id(off) edit directly, with no second FIND to learn the rev.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
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
    /// Content rev of that node — what `IF REV` takes.
    ///
    /// Handed out with the handle, never apart from it: an outline row is a row
    /// you mutate from, and making it fetch the rev separately would cost a
    /// round trip per edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
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
    /// Stable node handle of the member declaration.
    ///
    /// Without it a member row was read-only: an agent that wanted to edit a
    /// field had to go back and FIND it by name. `None` for rows from legacy
    /// segments, which carry no ordinals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Content rev of that node — what `IF REV` takes. Always beside the handle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
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
    /// Number of `error_scope = 'root'` regions in this file — i.e. how badly
    /// tree-sitter failed to parse the file **as its declared language**.
    ///
    /// Deliberately NOT a count of every `ERROR` node. tree-sitter parses C
    /// without running the preprocessor, so `static ALWAYS_INLINE void f(void)`
    /// produces an ERROR beside the return type while `f` still indexes
    /// correctly. Zephyr has 21 681 such regions and 16 480 of them are `nested`
    /// — inside a node that indexed fine. Counting those would make `has_error`
    /// fire on idiomatic kernel C, and an alarm that goes off on healthy code is
    /// not an alarm. Only 207 of Zephyr's regions are `root`; those are the
    /// files that genuinely did not parse.
    ///
    /// For the raw picture use `FIND symbols WHERE fql_kind = 'error'` and
    /// filter on `error_scope`; for magnitude use `parse_coverage`.
    ///
    /// Populated **only** when the query filters, orders or groups on
    /// `has_error` / `error_count`: deriving it costs an index scan, so a plain
    /// `FIND files` never pays for it.  `None` therefore means *not asked for*,
    /// never *no errors* — do not read it as a clean bill of health.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_count: Option<u32>,
    /// Percent of the file's bytes tree-sitter parsed (0–100).
    ///
    /// `100 - (bytes inside ERROR regions / file size)`. Integer because the
    /// clause engine compares numbers as `i64`, so `WHERE parse_coverage < 50`
    /// works. This is the number that separates a macro-heavy but perfectly
    /// healthy C header (~99) from a file whose extension lies (~0).
    ///
    /// Populated only when a clause names it — see [`FileEntry::error_count`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parse_coverage: Option<u8>,
    /// Bare-hex handle (`n<hex>`) for this path — the whole-file (or
    /// whole-directory) node id. Present on path rows, absent on the aggregate
    /// rows a `GROUP BY` produces, which address nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Version stamp for the path, so a listed row is immediately actionable —
    /// `DELETE NODE '<node_id>' IF REV '<rev>'` in one round trip instead of a
    /// re-read. A file rev is the SHA-256 of its bytes; a directory rev is a
    /// membership XOR over the paths underneath it (it moves when the subtree
    /// gains or loses a file, not when a file's content changes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
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
