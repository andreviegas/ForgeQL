use std::path::PathBuf;
use thiserror::Error;

/// All error types produced by the `ForgeQL` core library.
///
/// Use `anyhow::Result` in binaries and tests.
/// Use `ForgeError` (via `thiserror`) inside this library so callers can
/// match on specific variants without parsing string messages.
#[derive(Debug, Error)]
pub enum ForgeError {
    // ---------------------------------------------------------------
    // Workspace
    // ---------------------------------------------------------------
    #[error("workspace root not found starting from '{0}'")]
    WorkspaceRootNotFound(PathBuf),

    #[error("path '{0}' is outside the workspace root")]
    PathOutsideWorkspace(PathBuf),

    // ---------------------------------------------------------------
    // AST / Parsing
    // ---------------------------------------------------------------
    #[error("failed to set tree-sitter language: {0}")]
    TreeSitterLanguage(String),

    #[error("tree-sitter failed to parse '{path}'")]
    AstParse { path: PathBuf },

    #[error("DSL parse error: {0}")]
    DslParse(String),

    // ---------------------------------------------------------------
    // Transforms
    // ---------------------------------------------------------------
    #[error("symbol '{name}' not found in index")]
    SymbolNotFound { name: String },

    #[error("transform plan is empty — nothing to do")]
    EmptyPlan,

    #[error("conflicting edits at byte range {start}..{end} in '{path}'")]
    ConflictingEdits {
        path: PathBuf,
        start: usize,
        end: usize,
    },

    // ---------------------------------------------------------------
    // File I/O
    // ---------------------------------------------------------------
    #[error("I/O error on '{path}': {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("atomic write failed for '{path}': could not persist tempfile")]
    AtomicPersist { path: PathBuf },

    // ---------------------------------------------------------------
    // Git
    // ---------------------------------------------------------------
    #[error("git error: {0}")]
    Git(#[from] git2::Error),

    // ---------------------------------------------------------------
    // Build verification
    // ---------------------------------------------------------------
    #[error("build step '{step}' failed with exit code {code}")]
    BuildFailed { step: String, code: i32 },

    #[error("build step '{step}' timed out after {secs}s")]
    BuildTimeout { step: String, secs: u64 },

    // ---------------------------------------------------------------
    // User input validation
    // ---------------------------------------------------------------
    #[error("{0}")]
    InvalidInput(String),

    // ---------------------------------------------------------------
    // Self-healing rejections
    // ---------------------------------------------------------------
    /// A rejection the caller can recover from by looking again. `Display`
    /// prints the `payload` alone — a JSON object for the structured kinds, a
    /// plain message for `NoSession` — so the rendered error is byte-identical
    /// to the old string; `kind` lets the engine classify it without parsing.
    #[error("{payload}")]
    Rejection {
        kind: RejectionKind,
        payload: String,
    },
}

/// The taxonomy of self-healing rejections.
///
/// Carried on [`ForgeError::Rejection`]. Deliberately coarse — the payload
/// holds the details; this only names the recovery family so the engine can
/// classify a rejection without parsing its message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectionKind {
    /// A rev guard (single-node or FOUND-set) did not match the live state.
    RevMismatch,
    /// A handle resolved to no node.
    NodeNotFound,
    /// A session-dependent command ran with no session.
    NoSession,
    /// A bulk `FOUND` verb could not proceed — no armed set, a truncated
    /// arming FIND (so no master rev), or a missing `IF REV`.
    FoundRefusedNoLimit,
}

/// Convenience constructor: wrap a `std::io::Error` with the offending path.
impl ForgeError {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
