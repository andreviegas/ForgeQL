/// The `ForgeQL` Intermediate Representation.
///
/// Both the pest DSL parser and the JSON-RPC handler produce `ForgeQLIR`.
/// There is one execution path, not two.
///
/// `#[serde(tag = "op")]` means the JSON wire format looks like:
/// ```json
///   { "op": "rename_symbol", "from": "acenderLuz", "to": "turnOnLight" }
/// ```
use serde::{Deserialize, Serialize};

// -----------------------------------------------------------------------
// Clause types — shared by all read-only query operations
// -----------------------------------------------------------------------

/// Sort direction for `ORDER BY` clauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortDirection {
    Asc,
    Desc,
}

/// An `ORDER BY` clause: sort results by `field` in the given `direction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderBy {
    /// The field name to sort by (e.g. `"usages"`, `"name"`).
    pub field: String,
    /// Ascending or descending.
    pub direction: SortDirection,
}

/// Comparison operator for `WHERE` predicates.
///
/// Supports six relational operators plus SQL-style `LIKE` / `NOT LIKE`
/// string pattern matching (where `%` matches any sequence of characters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Eq,
    NotEq,
    Like,
    NotLike,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// Right-hand-side value of a `Predicate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PredicateValue {
    String(String),
    /// Signed integer (used for `=`, `!=`, `>`, `>=`, `<`, `<=`).
    Number(i64),
    /// Boolean (used for `= true` / `= false`).
    Bool(bool),
}

/// A single `WHERE` or `HAVING` predicate: `<field> <op> <value>`.
///
/// Example: `WHERE usages >= 5` becomes
/// `Predicate { field: "usages".into(), op: Gte, value: Number(5) }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predicate {
    /// Field name to compare (e.g. `"name"`, `"node_kind"`, `"path"`,
    /// `"line"`, `"usages"`, or any dynamic field stored on the row).
    pub field: String,
    /// Comparison operator.
    pub op: CompareOp,
    /// Right-hand-side value.
    pub value: PredicateValue,
}

/// `GROUP BY` clause — group results by a named field before `HAVING`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GroupBy {
    /// Group by the value of arbitrary named field (e.g. `"file"`, `"kind"`).
    Field(String),
}

/// Universal clause set for all read-only query operations.
///
/// Replaces the old `QueryFilter` struct.  All filtering, sorting,
/// grouping, and pagination is expressed as typed clauses here.
///
/// Embedded via `#[serde(flatten)]` in each query IR variant so the JSON
/// wire format stays flat:
/// `{"op":"find_symbols","pattern":"set%","exclude_glob":"tests/**"}`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Clauses {
    /// `WHERE <field> <op> <value>` predicates — all must match (AND semantics).
    ///
    /// Replaces `kind_filter`, `name_like`, `sig_like`, `sig_not_like`,
    /// `globals_only`, and `numeric_predicates` from the old `QueryFilter`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub where_predicates: Vec<Predicate>,

    /// `HAVING <field> <op> <value>` predicates — applied after `GROUP BY`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub having_predicates: Vec<Predicate>,

    /// `IN 'glob'` — restrict to files matching this glob (was `path_glob`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_glob: Option<String>,

    /// `EXCLUDE 'glob'` — remove files matching this glob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_glob: Option<String>,

    /// `ORDER BY <field> [ASC|DESC]` — sort before `LIMIT`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_by: Option<OrderBy>,

    /// `GROUP BY <field>` — group results by a named field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_by: Option<GroupBy>,

    /// `LIMIT N` — return at most `N` results after ordering.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,

    /// `OFFSET N` — skip first `N` results (for pagination).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<usize>,

    /// `DEPTH N` — collapse AST blocks deeper than `N` levels (SHOW BODY).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ForgeQLIR {
    // ------------------------------------------------------------------
    // Source / session management
    // ------------------------------------------------------------------
    /// `CREATE SOURCE 'name' FROM 'url'` — bare-clone a remote repository.
    CreateSource { name: String, url: String },

    /// `REFRESH SOURCE 'name'` — fetch all remotes on an existing bare repository.
    ///
    /// This brings a previously cloned source up to date with its upstream without
    /// requiring a full re-clone.  Any in-memory sessions whose branch HEAD has
    /// moved will be re-indexed automatically on the next command.
    RefreshSource { name: String },

    /// `USE source.branch [AS 'custom-branch']` — create (or resume) a session.
    ///
    /// `as_branch`, when present, is used as the git branch name instead of the
    /// auto-generated `forgeql/<session_id>`.  Allows the caller to create a
    /// human-readable branch (e.g. `agent/refactor-signal-api`) that can be
    /// fetched and reviewed by the senior developer without decoding session IDs.
    UseSource {
        source: String,
        branch: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        as_branch: Option<String>,
    },

    /// `SHOW SOURCES` — list all registered sources.
    ShowSources,

    /// `SHOW BRANCHES [OF 'source']` — list branches of a source.
    ShowBranches {
        #[serde(skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },

    /// `DISCONNECT` — end the current session, delete its worktree and
    ///  per-session git branch.  `session_id` is supplied via the HTTP params
    ///  (same as FIND / RENAME), not embedded in the DSL statement.
    Disconnect,

    // ------------------------------------------------------------------
    // Queries (read-only)
    // ------------------------------------------------------------------
    FindSymbols {
        #[serde(flatten)]
        clauses: Clauses,
    },

    FindUsages {
        of: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `FIND files [IN 'glob'] [EXCLUDE 'glob'] [DEPTH n]`
    /// Enumerates workspace files matching `in_glob`, returning path, size,
    /// and extension for each match.  When `depth` is set, results are
    /// grouped by directory: sub-folders deeper than `depth` levels
    /// relative to the IN root are collapsed into summary entries
    /// showing only the folder name and file count.
    FindFiles {
        #[serde(flatten)]
        clauses: Clauses,
    },

    // ------------------------------------------------------------------
    // Code Exposure API (§1)
    // ------------------------------------------------------------------
    /// `SHOW context OF 'symbol' [IN 'file'] [LINES n]`
    /// Returns ±N source lines around the symbol's definition site.
    /// `clauses.in_glob` restricts to a specific file; `clauses.depth`
    /// sets the context window size (default 5).
    ShowContext {
        symbol: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW signature OF 'symbol'`
    /// Returns the function/type signature (up to `{` or `;`), all overloads.
    ShowSignature {
        symbol: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW outline OF 'file'`
    /// Returns all top-level declarations in the file in source order.
    ShowOutline {
        file: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW members OF 'ClassName'`
    /// Returns field names and method signatures for a struct/class.
    ShowMembers {
        symbol: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW body OF 'func' [DEPTH n]`
    /// Returns the full function definition.  With `DEPTH n`, blocks nested
    /// deeper than `n` levels are collapsed to `{ ... }`.
    /// `clauses.depth` carries the collapse level.
    ShowBody {
        symbol: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW callees OF 'func'`
    /// Returns all function names called within the body of `func`.
    ShowCallees {
        symbol: String,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW LINES n-m OF 'file'`
    /// Returns source text for lines `start_line`..=`end_line` (1-based,
    /// inclusive), with per-line annotations.
    ShowLines {
        file: String,
        start_line: usize,
        end_line: usize,
        #[serde(flatten)]
        clauses: Clauses,
    },

    // ------------------------------------------------------------------
    // Mutations
    // ------------------------------------------------------------------
    /// `CHANGE FILE[S] ... <target>` — universal mutation command.
    ///
    /// One command covers creation, modification, deletion, and file-scoped
    /// rename. The `target` discriminates the mode; `files` lists the paths.
    ChangeContent {
        files: Vec<String>,
        target: ChangeTarget,
        #[serde(flatten)]
        clauses: Clauses,
    },

    // ------------------------------------------------------------------
    // Checkpoint-based transactions
    // ------------------------------------------------------------------
    /// `BEGIN TRANSACTION 'name'` — create a named git checkpoint.
    BeginTransaction { name: String },

    /// `COMMIT MESSAGE 'msg'` — stage all changes and create a git commit.
    Commit { message: String },

    /// `ROLLBACK [TRANSACTION 'name']` — revert to a named checkpoint
    /// via `git reset --hard`.
    Rollback {
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// `VERIFY build 'step'` — run a named verify step from `.forgeql.yaml`
    /// as a standalone command (outside a transaction).
    VerifyBuild {
        /// Name of the verify step to run.
        step: String,
    },
}

/// Targeting mode for the `CHANGE FILE[S]` command.
///
/// Each variant resolves to one or more `ByteRangeEdit`s at plan time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ChangeTarget {
    /// `WITH '...'` — create file (if absent) or overwrite its entire content.
    WithContent { content: String },
    /// `MATCHING 'text' WITH '...'` — find a unique text match and replace it.
    Matching {
        pattern: String,
        replacement: String,
    },
    /// `LINES n-m WITH '...'` — replace a 1-based inclusive line range.
    Lines {
        start: usize,
        end: usize,
        content: String,
    },
    /// `WITH NOTHING` — delete the listed files.
    Delete,
}
