/// `ForgeQL` MCP server handler.
///
/// Implements the Model Context Protocol (MCP) using `rmcp`.  Each `ForgeQL`
/// operation is exposed as an MCP tool.  The primary tool is `run_fql` which
/// accepts raw FQL and delegates to `ForgeQLEngine::execute()`.
use std::sync::Mutex;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use tracing::{debug, error};

use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::error::ForgeError;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::result::ForgeQLResult;

// -----------------------------------------------------------------------
// MCP handler struct
// -----------------------------------------------------------------------

/// MCP server handler wrapping a `ForgeQLEngine`.
///
/// The engine is behind a `Mutex` because `ServerHandler` requires `Sync`
/// while `ForgeQLEngine::execute` takes `&mut self`.  Stdio transport
/// processes one request at a time, so contention is negligible.
pub(crate) struct ForgeQlMcp {
    engine: Mutex<ForgeQLEngine>,
    tool_router: ToolRouter<Self>,
    logger: Mutex<Option<crate::QueryLogger>>,
}

impl ForgeQlMcp {
    /// Create a new MCP handler wrapping the given engine.
    pub(crate) fn new(engine: ForgeQLEngine, logger: Option<crate::QueryLogger>) -> Self {
        Self {
            engine: Mutex::new(engine),
            tool_router: Self::tool_router(),
            logger: Mutex::new(logger),
        }
    }

    /// Append a log row for a completed FQL statement (no-op when logger is disabled).
    fn log_query(&self, fql: &str, result: &ForgeQLResult, output: &str) {
        if let Ok(mut guard) = self.logger.lock()
            && let Some(ref mut l) = *guard
        {
            l.log(fql, result, output);
        }
    }

    /// Update the logger's source name (called after a successful USE).
    fn set_log_source(&self, source: &str) {
        if let Ok(mut guard) = self.logger.lock()
            && let Some(ref mut l) = *guard
        {
            l.set_source(source);
        }
    }
}

// -----------------------------------------------------------------------
// Tool parameter structs
// -----------------------------------------------------------------------

/// Output format for `run_fql` responses.
///
/// `Csv` (default) — compact flat-array rows; ~60% fewer tokens than JSON.
/// Non-query results (mutations, SHOW, source ops) always use JSON regardless.
/// `Json` — full structured JSON; use only when you need to parse specific fields.
#[derive(Debug, Deserialize, schemars::JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum OutputFormat {
    #[default]
    Csv,
    Json,
}

/// Parameters for the `run_fql` tool — execute any `ForgeQL` statement.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct RunFqlParams {
    /// The `ForgeQL` statement to execute (e.g. "FIND symbols WHERE name LIKE 'set%'").
    pub fql: String,
    /// Session ID from a previous USE command.  Required for queries
    /// and mutations; optional for CREATE SOURCE.
    pub session_id: Option<String>,
    /// Output format: "CSV" (default, ~60% fewer tokens) or "JSON" (structured parsing).
    /// CSV format: `{"total": N, "results": [["name","kind","path","count"], ...]}`.
    /// The `count` column holds `usages_count` for FIND results and the
    /// per-file hit count for `COUNT … GROUP BY file` results.
    /// Non-query results (mutations, SHOW, source ops) always return JSON.
    pub format: Option<OutputFormat>,
}

/// Parameters for `use_source` — start or resume a session.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct UseSourceParams {
    /// Source name (registered via `create_source`).
    pub source: String,
    /// Branch to check out (e.g. "main").
    pub branch: String,
    /// Optional custom branch alias (e.g. "agent/refactor-signal-api").
    pub as_branch: Option<String>,
}

/// Parameters for `find_symbols` — search symbols by name pattern.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct FindSymbolsParams {
    /// Glob pattern to match symbol names (e.g. "set%").
    pub pattern: String,
    /// Session ID (required).
    pub session_id: String,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// Parameters for `find_usages` — find all usages of a symbol.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct FindUsagesParams {
    /// Exact symbol name to find usages of.
    pub symbol: String,
    /// Session ID (required).
    pub session_id: String,
}

/// Parameters for `show_body` — show the body of a function/method.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ShowBodyParams {
    /// Symbol name to show the body of.
    pub symbol: String,
    /// Session ID (required).
    pub session_id: String,
    /// Collapse depth (0 = signature only, default; 1+ = progressive body reveal).
    pub depth: Option<usize>,
}

/// Parameters for `disconnect` — end a session and clean up.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct DisconnectParams {
    /// Session ID to disconnect.
    pub session_id: String,
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Convert a `ForgeError` (from the parser) to an `rmcp` `ErrorData`.
///
/// Takes ownership because `map_err` passes the error by value.
#[allow(clippy::needless_pass_by_value)]
fn parse_error(err: ForgeError) -> ErrorData {
    ErrorData::internal_error(format!("{err:#}"), None)
}

/// Convert an `anyhow::Error` (from the engine) to an `rmcp` `ErrorData`.
///
/// Takes ownership because `map_err` passes the error by value.
#[allow(clippy::needless_pass_by_value)]
fn engine_error(err: anyhow::Error) -> ErrorData {
    ErrorData::internal_error(format!("{err:#}"), None)
}

/// Build a successful `CallToolResult` containing JSON text.
fn json_result(json: &str) -> CallToolResult {
    CallToolResult::success(vec![Content::text(json)])
}

/// Append server-side metadata to a `ForgeQL` JSON response string.
///
/// Injects `tokens_approx` — an estimate of how many tokens the response
/// will consume (1 token ≈ 4 UTF-8 bytes).  The agent can use this to
/// decide whether to narrow the query before widening.
///
/// All `ForgeQL` responses are single top-level JSON objects, so we splice
/// the field in before the closing `}`.
fn append_meta(json: String) -> String {
    /// Approximate number of UTF-8 characters per LLM token.
    const CHARS_PER_TOKEN: usize = 4;
    let tokens_approx = json.len().div_ceil(CHARS_PER_TOKEN);
    if json.ends_with('}') {
        format!(
            "{},\"tokens_approx\":{tokens_approx}}}",
            &json[..json.len() - 1]
        )
    } else {
        json
    }
}

/// Execute a parsed `ForgeQLIR` operation and return the raw result.
///
/// Callers are responsible for serializing to the desired output format.
///
/// # Panic safety
///
/// The engine is held under a `std::sync::Mutex`.  Any panic inside
/// `execute()` would normally poison the lock, making all subsequent requests
/// fail with "engine lock poisoned".  This function prevents that in two ways:
///
/// 1. **Poison recovery**: if a previous call already poisoned the lock, the
///    guard is recovered via `into_inner()` so new requests can proceed.
/// 2. **`catch_unwind`**: the `execute()` call is wrapped in `catch_unwind`
///    so that panics are caught and converted to error responses before the
///    guard is dropped, keeping the mutex un-poisoned.
fn exec_engine(
    engine: &Mutex<ForgeQLEngine>,
    session_id: Option<&str>,
    op: &ForgeQLIR,
) -> Result<ForgeQLResult, ErrorData> {
    // Acquire the lock, recovering from poison if a previous request panicked.
    let mut guard = match engine.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            error!("engine mutex was poisoned — recovering guard; previous request panicked");
            poisoned.into_inner()
        }
    };

    // Wrap execute() in catch_unwind to prevent future poisoning.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        guard.execute(session_id, op)
    }))
    .map_err(|payload| {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| format!("engine panicked: {s}"))
            .or_else(|| {
                payload
                    .downcast_ref::<String>()
                    .map(|s| format!("engine panicked: {s}"))
            })
            .unwrap_or_else(|| "engine panicked: unknown cause".to_string());
        error!(%msg, "engine panic caught — converting to error response");
        ErrorData::internal_error(msg, None)
    })?
    .map_err(engine_error)?;

    drop(guard);
    Ok(result)
}

/// Execute a parsed `ForgeQLIR` operation and return a JSON `CallToolResult`.
fn run_engine(
    engine: &Mutex<ForgeQLEngine>,
    session_id: Option<&str>,
    op: &ForgeQLIR,
) -> Result<CallToolResult, ErrorData> {
    Ok(json_result(&exec_engine(engine, session_id, op)?.to_json()))
}

// -----------------------------------------------------------------------
// Tool definitions — the `#[tool_router]` macro scans these
// -----------------------------------------------------------------------

#[tool_router]
impl ForgeQlMcp {
    /// Execute any `ForgeQL` statement.
    ///
    /// This is the primary tool — it accepts raw FQL syntax and returns
    /// structured JSON results.  All `ForgeQL` operations are supported.
    #[tool(
        name = "run_fql",
        description = "Execute any ForgeQL statement (FIND, SHOW, RENAME, etc.). OUTPUT: format defaults to CSV (pass format=JSON only when parsing fields programmatically). LIMIT: FIND queries without LIMIT default to 20 rows; add LIMIT N to override; when total > results.len() more rows exist. WORKFLOW: start narrow (WHERE/IN/LIMIT), verify, then widen."
    )]
    fn run_fql(
        &self,
        Parameters(params): Parameters<RunFqlParams>,
    ) -> Result<CallToolResult, ErrorData> {
        debug!(fql = %params.fql, format = ?params.format, "run_fql");
        let ops = parser::parse_with_source(&params.fql).map_err(parse_error)?;
        if ops.is_empty() {
            return Err(ErrorData::invalid_params("empty FQL statement", None));
        }
        // Validate all ops before executing any — fail fast on forbidden ops.
        for (_, op) in &ops {
            if matches!(
                op,
                ForgeQLIR::CreateSource { .. } | ForgeQLIR::RefreshSource { .. }
            ) {
                return Err(ErrorData::invalid_params(
                    "CREATE SOURCE and REFRESH SOURCE are not permitted via MCP. \
                     Sources are managed by the server administrator. \
                     Use USE to connect to an existing source.",
                    None,
                ));
            }
        }
        // Execute each statement individually — one log row per command,
        // exactly as if each were sent in a separate run_fql call.
        let format = params.format.unwrap_or_default();
        let mut outputs: Vec<String> = Vec::with_capacity(ops.len());
        for (source_text, op) in &ops {
            let result = exec_engine(&self.engine, params.session_id.as_deref(), op)?;
            if let ForgeQLIR::UseSource { source, .. } = op {
                self.set_log_source(source);
            }
            let output = match format {
                OutputFormat::Csv => result.to_csv(),
                OutputFormat::Json => result.to_json(),
            };
            let output = append_meta(output);
            self.log_query(source_text, &result, &output);
            outputs.push(output);
        }
        // Single statement → return its result directly (no wrapping).
        // Multiple statements → return a JSON array so the agent sees every result.
        if let [single] = outputs.as_slice() {
            Ok(json_result(single))
        } else {
            let combined = format!("[{}]", outputs.join(","));
            Ok(json_result(&combined))
        }
    }

    /// Start or resume a session on a source branch.
    #[tool(
        name = "use_source",
        description = "Start or resume a session on a source branch (returns session_id)"
    )]
    fn use_source(
        &self,
        Parameters(params): Parameters<UseSourceParams>,
    ) -> Result<CallToolResult, ErrorData> {
        debug!(source = %params.source, branch = %params.branch, "use_source");
        let fql = if let Some(ref alias) = params.as_branch {
            format!("USE {}.{} AS '{alias}'", params.source, params.branch)
        } else {
            format!("USE {}.{}", params.source, params.branch)
        };
        let op = ForgeQLIR::UseSource {
            source: params.source.clone(),
            branch: params.branch,
            as_branch: params.as_branch,
        };
        let result = exec_engine(&self.engine, None, &op)?;
        self.set_log_source(&params.source);
        let output = result.to_json();
        self.log_query(&fql, &result, &output);
        Ok(json_result(&output))
    }

    /// Search for symbols matching a name pattern.
    #[tool(
        name = "find_symbols",
        description = "Find symbols matching a name pattern (e.g. 'set%'). Defaults to LIMIT 20; pass limit to override. For full query flexibility (WHERE, ORDER BY, etc.) use run_fql."
    )]
    fn find_symbols(
        &self,
        Parameters(params): Parameters<FindSymbolsParams>,
    ) -> Result<CallToolResult, ErrorData> {
        debug!(pattern = %params.pattern, "find_symbols");
        let fql = if let Some(limit) = params.limit {
            format!(
                "FIND symbols WHERE name LIKE '{}' LIMIT {limit}",
                params.pattern
            )
        } else {
            format!("FIND symbols WHERE name LIKE '{}'", params.pattern)
        };
        let ops = parser::parse(&fql).map_err(parse_error)?;
        let op = ops
            .first()
            .ok_or_else(|| ErrorData::internal_error("parse returned empty", None))?;
        run_engine(&self.engine, Some(&params.session_id), op)
    }

    /// Find all usages of a symbol across the codebase.
    #[tool(
        name = "find_usages",
        description = "Find all usages of a symbol across the indexed codebase. Results are capped at 20; use run_fql with LIMIT N to override."
    )]
    fn find_usages(
        &self,
        Parameters(params): Parameters<FindUsagesParams>,
    ) -> Result<CallToolResult, ErrorData> {
        debug!(symbol = %params.symbol, "find_usages");
        let fql = format!("FIND usages OF '{}'", params.symbol);
        let ops = parser::parse(&fql).map_err(parse_error)?;
        let op = ops
            .first()
            .ok_or_else(|| ErrorData::internal_error("parse returned empty", None))?;
        run_engine(&self.engine, Some(&params.session_id), op)
    }

    /// Show the body of a function or method.
    #[tool(
        name = "show_body",
        description = "Show the body of a function or method. Default depth=0 returns signature only. Use depth=1 to see one level of structure, depth=2 for more detail, etc. Never skip directly to a high depth for large functions."
    )]
    fn show_body(
        &self,
        Parameters(params): Parameters<ShowBodyParams>,
    ) -> Result<CallToolResult, ErrorData> {
        debug!(symbol = %params.symbol, depth = ?params.depth, "show_body");
        let fql = if let Some(depth) = params.depth {
            format!("SHOW body OF '{}' DEPTH {depth}", params.symbol)
        } else {
            format!("SHOW body OF '{}'", params.symbol)
        };
        let ops = parser::parse(&fql).map_err(parse_error)?;
        let op = ops
            .first()
            .ok_or_else(|| ErrorData::internal_error("parse returned empty", None))?;
        run_engine(&self.engine, Some(&params.session_id), op)
    }

    /// End a session and clean up its worktree.
    #[tool(
        name = "disconnect",
        description = "End a session and clean up its worktree and branch"
    )]
    fn disconnect(
        &self,
        Parameters(params): Parameters<DisconnectParams>,
    ) -> Result<CallToolResult, ErrorData> {
        debug!(session_id = %params.session_id, "disconnect");
        let op = ForgeQLIR::Disconnect;
        run_engine(&self.engine, Some(&params.session_id), &op)
    }
}

// -----------------------------------------------------------------------
// ServerHandler — `#[tool_handler]` wires up call_tool / list_tools
// -----------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ForgeQlMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_instructions(
            "ForgeQL — AST-aware code transformation server. \
             Use `run_fql` to execute any ForgeQL statement, or use \
             the individual tools for structured access. \
             EFFICIENCY RULES: (1) format defaults to CSV — use JSON only when parsing fields. \
             (2) FIND queries default to 20 rows; always add LIMIT before broadening. \
             (3) Every response includes tokens_approx — if it is large, narrow the query. \
             (4) SHOW body defaults to DEPTH 0 (signature only); increment depth to reveal structure progressively.",
        )
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unwrap_in_result)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("tests/fixtures")
    }

    fn mcp_with_session() -> (ForgeQlMcp, String, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let src = fixtures_dir();
        let _ = fs::copy(
            src.join("motor_control.h"),
            dir.path().join("motor_control.h"),
        )
        .expect("copy .h");
        let _ = fs::copy(
            src.join("motor_control.cpp"),
            dir.path().join("motor_control.cpp"),
        )
        .expect("copy .cpp");

        let data_dir = dir.path().join("data");
        let mut engine = ForgeQLEngine::new(data_dir).expect("engine");
        let session_id = engine
            .register_local_session(dir.path())
            .expect("register session");
        let mcp = ForgeQlMcp::new(engine, None);
        (mcp, session_id, dir)
    }

    fn first_text(result: &CallToolResult) -> &str {
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text
            .as_str()
    }

    #[test]
    fn get_info_returns_tools_capability() {
        let tmp = tempdir().unwrap();
        let engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
        let mcp = ForgeQlMcp::new(engine, None);
        let info = mcp.get_info();
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn get_info_has_instructions() {
        let tmp = tempdir().unwrap();
        let engine = ForgeQLEngine::new(tmp.path().to_path_buf()).unwrap();
        let mcp = ForgeQlMcp::new(engine, None);
        let info = mcp.get_info();
        let instructions = info.instructions.expect("should have instructions");
        assert!(instructions.contains("ForgeQL"));
    }

    #[test]
    fn run_fql_find_symbols() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.run_fql(Parameters(RunFqlParams {
            fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
            session_id: Some(session_id),
            format: None,
        }));
        let call_result = result.expect("should succeed");
        let text = first_text(&call_result);
        assert!(
            text.contains("encenderMotor"),
            "JSON should contain symbol: {text}"
        );
    }

    #[test]
    fn run_fql_invalid_syntax_returns_error() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.run_fql(Parameters(RunFqlParams {
            fql: "NOT VALID FQL".to_string(),
            session_id: Some(session_id),
            format: None,
        }));
        assert!(result.is_err(), "invalid FQL should return ErrorData");
    }

    #[test]
    fn run_fql_create_source_is_blocked() {
        let (mcp, _session_id, _dir) = mcp_with_session();
        let result = mcp.run_fql(Parameters(RunFqlParams {
            fql: "CREATE SOURCE 'evil' FROM 'https://example.com/repo.git'".to_string(),
            session_id: None,
            format: None,
        }));
        assert!(result.is_err(), "CREATE SOURCE via MCP must be rejected");
        let err = result.unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not permitted") || msg.contains("administrator"),
            "error should mention admin restriction: {msg}"
        );
    }

    #[test]
    fn run_fql_csv_format_returns_flat_rows() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.run_fql(Parameters(RunFqlParams {
            fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
            session_id: Some(session_id),
            format: Some(OutputFormat::Csv),
        }));
        let call_result = result.expect("should succeed");
        let text = first_text(&call_result);
        // CSV envelope has "total" and "results" as flat arrays, no "op" key.
        assert!(
            text.contains("\"total\""),
            "CSV output should have total field: {text}"
        );
        assert!(
            text.contains("\"results\""),
            "CSV output should have results field: {text}"
        );
        assert!(
            !text.contains("\"op\""),
            "CSV output should not have op field: {text}"
        );
        assert!(
            text.contains("encenderMotor"),
            "CSV output should contain symbol name: {text}"
        );
    }

    #[test]
    fn find_symbols_tool_with_limit() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.find_symbols(Parameters(FindSymbolsParams {
            pattern: "%".to_string(),
            session_id,
            limit: Some(2),
        }));
        let call_result = result.expect("should succeed");
        assert!(!call_result.content.is_empty());
    }

    #[test]
    fn find_usages_tool() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.find_usages(Parameters(FindUsagesParams {
            symbol: "encenderMotor".to_string(),
            session_id,
        }));
        let call_result = result.expect("should succeed");
        assert!(!call_result.content.is_empty());
    }

    #[test]
    fn show_body_tool() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.show_body(Parameters(ShowBodyParams {
            symbol: "encenderMotor".to_string(),
            session_id,
            depth: None,
        }));
        let call_result = result.expect("should succeed");
        assert!(!call_result.content.is_empty());
    }
}
