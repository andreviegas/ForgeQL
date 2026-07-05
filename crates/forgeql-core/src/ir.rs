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
// Backend selector — produced by the optional USING clause
// -----------------------------------------------------------------------

/// Selects which storage backend serves a read-only query.
///
/// Produced by the optional `USING 'backend'` clause in FQL.
/// Mutations (`CHANGE`, `COPY`, `MOVE`) always write through all enabled
/// backends and do not accept a backend selector.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// No explicit `USING` clause — use the engine default (legacy for now).
    #[default]
    Default,
    /// `USING 'legacy'` — explicitly route to the `LegacyMemoryStorage` backend.
    Legacy,
    /// `USING 'columnar'` — route to the columnar backend.
    ///
    /// Returns a "not enabled" error if no columnar backend is installed for the session.
    Columnar,
}

impl Backend {
    /// Parse a backend name from a `USING 'name'` clause.
    ///
    /// # Errors
    /// Returns `Err` if the name is not a known backend.
    pub fn from_clause(s: &str) -> Result<Self, crate::error::ForgeError> {
        match s {
            "legacy" => Ok(Self::Legacy),
            "columnar" => Ok(Self::Columnar),
            other => Err(crate::error::ForgeError::DslParse(format!(
                "unknown backend '{other}'; known backends: legacy, columnar"
            ))),
        }
    }
}
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
/// Supports six relational operators, SQL-style `LIKE` / `NOT LIKE`
/// pattern matching, and `MATCHES` / `NOT MATCHES` for regex filtering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Eq,
    NotEq,
    Like,
    NotLike,
    /// `WHERE field MATCHES 'regex'` — full regex match via the `regex` crate.
    Matches,
    /// `WHERE field NOT MATCHES 'regex'` — negated regex match.
    NotMatches,
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
    /// Group by the value of arbitrary named field (e.g. `"file"`, `"fql_kind"`).
    Field(String),
}

/// Universal clause set for all read-only query operations.
///
/// Replaces the old `QueryFilter` struct.  All filtering, sorting,
/// grouping, and pagination is expressed as typed clauses here.
///
/// Embedded via `#[serde(flatten)]` in each query IR variant so the JSON
/// wire format stays flat:
/// `{"op":"find_symbols","pattern":"set%","exclude_globs":["tests/**"]}`.
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

    /// `EXCLUDE <glob>` clauses — remove files matching ANY of these globs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_globs: Vec<String>,

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
    /// `as_branch` is the git branch name used for this session instead of the
    /// auto-generated `forgeql/<session_id>`.  Allows the caller to create a
    /// human-readable branch (e.g. `agent/refactor-signal-api`) that can be
    /// fetched and reviewed by the senior developer without decoding session IDs.
    /// This field is mandatory — every USE command must supply AS 'branch-name'.
    UseSource {
        source: String,
        branch: String,
        as_branch: String,
    },

    /// `SHOW SOURCES` — list all registered sources.
    ShowSources,

    /// `SHOW BRANCHES` — list branches of the current session source.
    /// `SHOW BRANCHES` — list branches of the current session source.
    ShowBranches,

    /// `SHOW STATS [FOR 'session_id']` — report internal stats for one or all sessions.
    /// When `session_id` is `None`, reports aggregate stats across all loaded sessions.
    ShowStats {
        /// Optional session alias (the identifier after `AS` in `USE`).
        /// When `None`, aggregates across all active sessions.
        session_id: Option<String>,
    },

    // ------------------------------------------------------------------
    // Queries (read-only)
    // ------------------------------------------------------------------
    FindSymbols {
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    FindUsages {
        of: String,
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
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
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// FIND NODE id — resolve a `node_id` to its current location, rev, and nav links.
    /// `FIND NODE id` — resolve a `node_id` to its current location, rev, and nav links.
    FindNode { node_id: String },

    /// `CHANGE NODE 'id' [IF REV 'rev'] WITH content` — replace node source lines.
    ChangeNode {
        node_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        if_rev: Option<String>,
        content: String,
    },

    /// `INSERT BEFORE NODE 'id' WITH content` / `INSERT AFTER NODE 'id' WITH content`
    InsertNode {
        node_id: String,
        /// `true` = INSERT BEFORE, `false` = INSERT AFTER.
        before: bool,
        content: String,
    },

    /// `DELETE NODE 'id' [IF REV 'rev']` — delete node source lines.
    DeleteNode {
        node_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        if_rev: Option<String>,
    },

    /// `SHOW NODE 'id' [CONTENT | METADATA]`
    ///
    /// * `CONTENT` (default) — return the source lines of the node.
    /// * `METADATA` — return nav + location fields (same as `FIND NODE`).
    ShowNode {
        node_id: String,
        /// `false` = CONTENT (source lines, default), `true` = METADATA.
        metadata: bool,
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
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW signature OF 'symbol'`
    /// Returns the function/type signature (up to `{` or `;`), all overloads.
    ShowSignature {
        symbol: String,
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW outline OF 'file'`
    /// Returns all top-level declarations in the file in source order.
    ShowOutline {
        file: String,
        /// `ALL` — include every node, not only structural declarations.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW members OF 'ClassName'`
    /// Returns field names and method signatures for a struct/class.
    ShowMembers {
        symbol: String,
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW body OF 'func' [DEPTH n]`
    /// Returns the full function definition.  With `DEPTH n`, blocks nested
    /// deeper than `n` levels are collapsed to `{ ... }`.
    /// `clauses.depth` carries the collapse level.
    ShowBody {
        symbol: String,
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW callees OF 'func'`
    /// Returns all function names called within the body of `func`.
    ShowCallees {
        symbol: String,
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
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
        #[serde(default, skip_serializing_if = "crate::ir::is_default_backend")]
        backend: Backend,
        #[serde(flatten)]
        clauses: Clauses,
    },

    /// `SHOW MORE [HEAD n | TAIL n | n-m]` — page the session's last buffered output.
    ///
    /// Reads the `.forgeql-showmore` buffer written when a command's output
    /// exceeded the inline cap and returns the requested window. `WHERE text`
    /// and `LIMIT` from `clauses` filter the buffered lines (e.g. grep a build
    /// log with `SHOW MORE WHERE text MATCHES 'error'`).
    ShowMore {
        window: ShowMoreWindow,
        /// Which buffer in the `LAST-n` ring to page (0 = most recent). A bare
        /// `SHOW MORE` is `LAST-0`; `SHOW MORE LAST-1` pages the previous buffer.
        #[serde(default)]
        last: usize,
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

    /// `COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]`
    ///
    /// Reads lines `start`..=`end` (1-based, inclusive) from `src` and
    /// inserts them into `dst` before line `at` (1-based).  When `at` is
    /// `None` the lines are appended at the end of the file.
    ///
    /// `src` and `dst` may be the same file (useful for reordering blocks).
    CopyLines {
        src: String,
        start: usize,
        end: usize,
        dst: String,
        /// Destination insertion point (1-based line number).  `None` = append.
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<usize>,
    },

    /// `MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]`
    ///
    /// Identical to `CopyLines` but also deletes lines `start`..=`end` from
    /// `src` after the insertion.  For same-file moves the ordering of
    /// operations is chosen automatically to keep line numbers consistent.
    MoveLines {
        src: String,
        start: usize,
        end: usize,
        dst: String,
        /// Destination insertion point (1-based line number).  `None` = append.
        #[serde(skip_serializing_if = "Option::is_none")]
        at: Option<usize>,
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
        /// Positional arguments supplied after the step name; validated
        /// against the step's declared `params` at execution time.
        args: Vec<String>,
    },

    /// `RUN '<step>' <args…>` — run a named allowlisted command template from
    /// `.forgeql.yaml` `run_steps` (outside a transaction). `Ident` args are
    /// substituted into the command; `String` args are bound to the subprocess
    /// stdin.
    Run {
        /// Name of the run step (template) to execute.
        step: String,
        /// Positional arguments supplied after the step name; validated against
        /// the template's declared `params` at execution time.
        args: Vec<String>,
    },

    /// `UNDO [LAST-n]` — restore the files a recent mutation changed to their
    /// pre-edit bytes. Reverses mutations from the per-session undo ring; `last`
    /// selects the slot (0 = most recent mutation, the default).
    Undo {
        /// Which ring slot to restore (0 = most recent; `LAST-n` = n).
        #[serde(default)]
        last: usize,
    },
    /// `JOB START '<label>'` — run a verify step as a detached background job,
    /// returning a job id immediately (the build does not block the request).
    JobStart {
        /// Verify-step label to run (same labels as `VERIFY build`).
        label: String,
    },
    /// `JOB STATUS '<id>'` — poll a background job's state and output.
    JobStatus {
        /// Job id returned by `JOB START`.
        id: String,
    },
    /// `JOB LIST` — list all known background jobs.
    JobList,
}

/// Serde helper: skip-serializing `Backend` when it holds the `Default` variant.
///
/// Used in all read-only `ForgeQLIR` variants so that the JSON wire format
/// is unchanged for queries without a `USING` clause.
#[must_use]
pub fn is_default_backend(b: &Backend) -> bool {
    *b == Backend::Default
}

/// Window selector for the `SHOW MORE` command.
///
/// `Full` (no window given) returns the whole buffer; the others bound it to a
/// slice of the session's last buffered output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShowMoreWindow {
    /// No window given — return the entire buffer.
    Full,
    /// `HEAD n` — the first `n` lines.
    Head(usize),
    /// `TAIL n` — the last `n` lines.
    Tail(usize),
    /// `n-m` — the 1-based inclusive line range.
    Range(usize, usize),
}

/// Targeting mode for the `CHANGE FILE[S]` command.
///
/// Each variant resolves to one or more `ByteRangeEdit`s at plan time.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ChangeTarget {
    /// `WITH '...'` — create file (if absent) or overwrite its entire content.
    WithContent { content: String },
    /// `MATCHING [WORD] 'text' WITH '...'` — find text matches and replace them.
    /// When `word_boundary` is true the pattern is wrapped in `\b...\b` so
    /// that only whole-word occurrences are replaced.
    Matching {
        pattern: String,
        replacement: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        word_boundary: bool,
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
