/// `ForgeQL` MCP server handler.
///
/// Implements the Model Context Protocol (MCP) using `rmcp`.  Each `ForgeQL`
/// operation is exposed as an MCP tool.  The primary tool is `run_fql` which
/// accepts raw FQL and delegates to `ForgeQLEngine::execute()`.
use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use tracing::{debug, error};

use forgeql_core::compact;
use forgeql_core::engine::{
    DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, DEFAULT_QUERY_LIMIT, DEFAULT_SHOW_LINE_LIMIT,
    ForgeQLEngine,
};
use forgeql_core::error::ForgeError;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::query_logger::QueryLogger;
use forgeql_core::result::ForgeQLResult;

// -----------------------------------------------------------------------
// MCP handler struct
// -----------------------------------------------------------------------

/// MCP server handler wrapping a `ForgeQLEngine`.
///
/// The engine is behind an `Arc<Mutex>` because `ServerHandler` requires
/// `Sync` while `ForgeQLEngine::execute` takes `&mut self`.  The `Arc`
/// allows sharing the engine with a background eviction task.
pub(crate) struct ForgeQlMcp {
    engine: Arc<Mutex<ForgeQLEngine>>,
    tool_router: ToolRouter<Self>,
    logger: Mutex<Option<QueryLogger>>,
}

impl ForgeQlMcp {
    /// Create a new MCP handler wrapping the given engine.
    pub(crate) fn new(engine: Arc<Mutex<ForgeQLEngine>>, logger: Option<QueryLogger>) -> Self {
        Self {
            engine,
            tool_router: Self::tool_router(),
            logger: Mutex::new(logger),
        }
    }

    /// Append a log row for a completed FQL statement (no-op when logger is disabled).
    fn log_query(
        &self,
        fql: &str,
        result: &ForgeQLResult,
        output: &str,
        elapsed_ms: u64,
        source: &str,
    ) {
        if let Ok(mut guard) = self.logger.lock()
            && let Some(ref mut l) = *guard
        {
            l.log(fql, result, output, elapsed_ms, source);
        }
    }

    /// Resolve the source name for a session from the engine.
    fn resolve_source(&self, session_id: Option<&str>) -> String {
        session_id
            .and_then(|sid| {
                self.engine
                    .lock()
                    .ok()
                    .and_then(|g| g.source_name_for_session(sid).map(str::to_owned))
            })
            .unwrap_or_else(|| "unknown".to_string())
    }
}

// -----------------------------------------------------------------------
// Tool parameter structs
// -----------------------------------------------------------------------

/// Output format for `run_fql` responses.
///
/// `Csv` (default) — compact grouped CSV; deduplicates repeated fields
/// (e.g. `node_kind` appears once per group, not per row).
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
    /// Output format: "CSV" (default, compact grouped CSV) or "JSON" (full structured).
    /// CSV groups repeated fields and drops derivable data for minimum token usage.
    /// Mutations, transactions, and source ops always return JSON.
    pub format: Option<OutputFormat>,
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
fn append_meta(output: &str) -> String {
    /// Approximate number of UTF-8 characters per LLM token.
    const CHARS_PER_TOKEN: usize = 4;
    let tokens_approx = output.len().div_ceil(CHARS_PER_TOKEN);
    output.strip_suffix('}').map_or_else(
        // Compact CSV — append as final row.
        || format!("{output}\n\"tokens_approx\",{tokens_approx}"),
        // JSON object — splice field before closing brace.
        |prefix| format!("{prefix},\"tokens_approx\":{tokens_approx}}}"),
    )
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
        let mut log_source = self.resolve_source(params.session_id.as_deref());
        let mut session_hint: Option<String> = None;
        let mut outputs: Vec<String> = Vec::with_capacity(ops.len());
        for (source_text, op) in &ops {
            let t0 = std::time::Instant::now();
            let result = exec_engine(&self.engine, params.session_id.as_deref(), op)?;
            let elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
            if let ForgeQLIR::UseSource { source, .. } = op {
                log_source.clone_from(source);
            }
            // Extract session_id from USE responses and build a hint for the agent.
            if session_hint.is_none()
                && let ForgeQLResult::SourceOp(ref sop) = result
                && let Some(sid) = sop.session_id.as_deref()
            {
                session_hint = Some(format!(
                    "⚠️ IMPORTANT: Pass session_id \"{sid}\" in ALL subsequent run_fql calls."
                ));
            }
            let output = match format {
                OutputFormat::Csv => compact::to_compact(&result),
                OutputFormat::Json => result.to_json(),
            };
            let output = append_meta(&output);
            self.log_query(source_text, &result, &output, elapsed_ms, &log_source);
            outputs.push(output);
        }
        // Single statement → return its result directly (no wrapping).
        // Multiple statements → return a JSON array so the agent sees every result.
        let body = if let [single] = outputs.as_slice() {
            single.clone()
        } else {
            format!("[{}]", outputs.join(","))
        };
        match session_hint {
            Some(hint) => Ok(CallToolResult::success(vec![
                Content::text(hint),
                Content::text(body),
            ])),
            None => Ok(json_result(&body)),
        }
    }
}

// -----------------------------------------------------------------------
// ServerHandler — `#[tool_handler]` wires up call_tool / list_tools
// -----------------------------------------------------------------------

#[tool_handler]
impl ServerHandler for ForgeQlMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            format!(
                "ForgeQL — AST-aware code transformation server.\n\
             All source code is accessed EXCLUSIVELY through ForgeQL queries.\n\
             \n\
             CRITICAL RULES:\n\
             - NEVER use SHOW LINES to explore code. \
               Use SHOW body OF 'symbol' DEPTH N instead.\n\
             - NEVER scan files sequentially or by line ranges. \
               Use FIND symbols WHERE to locate code by name, kind, or enrichment field.\n\
             - NEVER fall back to local filesystem tools (grep, find, cat, read_file). \
               ForgeQL manages all code access; the local workspace may be empty.\n\
             - Always start with USE source.branch before any query.\n\
             \n\
             QUERY STRATEGY (all commands accept WHERE, GROUP BY, ORDER BY, \
             LIMIT, OFFSET — combine them freely):\n\
             - Need a symbol? → FIND symbols WHERE name LIKE 'pattern' \
               [WHERE fql_kind = '...'] [IN 'path/**'] [ORDER BY usages DESC] [LIMIT N]\n\
             - Need source code? → SHOW body OF 'name' DEPTH {DEFAULT_BODY_DEPTH} (signature) \
               → DEPTH 1 (control flow) → DEPTH 99 (full source)\n\
             - Need blast radius? → FIND usages OF 'name' \
               [GROUP BY file] [ORDER BY count DESC] [LIMIT N]\n\
             - Need file list? → FIND files [IN 'path/**'] \
               [WHERE extension = '...'] [WHERE size > N] [ORDER BY size DESC] [LIMIT N]\n\
             - Need structure? → SHOW outline OF 'file' \
               [WHERE fql_kind = '...'] [ORDER BY line ASC] | SHOW members OF 'type'\n\
             - Need context? → SHOW context OF 'name' \
               (default {DEFAULT_CONTEXT_LINES} lines; use DEPTH N to adjust)\n\
             - Need call graph? → SHOW callees OF 'name' \
               | FIND usages OF 'name' GROUP BY file ORDER BY count DESC\n\
             \n\
             EFFICIENCY:\n\
             - Format defaults to CSV (≈60% fewer tokens). Pass format=JSON only when \
               parsing fields programmatically.\n\
             - FIND queries without LIMIT default to {DEFAULT_QUERY_LIMIT} rows. \
               Add LIMIT N to override. When total > results.len(), more rows exist.\n\
             - SHOW commands that return more than {DEFAULT_SHOW_LINE_LIMIT} source lines \
               WITHOUT an explicit LIMIT clause are BLOCKED — zero lines are returned, \
               only a guidance message. Use FIND symbols WHERE to locate the exact symbol \
               — it returns file path and line numbers — then SHOW LINES n-m OF 'file' \
               to read only those lines. Add LIMIT N only if you consciously need more \
               than {DEFAULT_SHOW_LINE_LIMIT} lines.\n\
             - Every response includes tokens_approx — if large, narrow the query \
               with WHERE, IN, EXCLUDE, or lower LIMIT.\n\
             - SHOW body defaults to DEPTH {DEFAULT_BODY_DEPTH} (signature only). \
               Increment depth progressively — never jump straight to DEPTH 99 for \
               large functions.\n\
             - Multiple WHERE clauses combine as AND — stack them to narrow results.\n\
             - Use GROUP BY (file | kind | node_kind) + HAVING count >= N to aggregate. \
               Never GROUP BY without HAVING on large codebases.\n\
             - Use OFFSET N for pagination when LIMIT truncates results.",
            ),
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
    use std::sync::Arc;

    use forgeql_core::ast::lang::LanguageRegistry;
    use forgeql_lang_cpp::CppLanguage;
    use tempfile::tempdir;

    fn make_registry() -> Arc<LanguageRegistry> {
        Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]))
    }

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
        let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
        let session_id = engine
            .register_local_session(dir.path())
            .expect("register session");
        let mcp = ForgeQlMcp::new(Arc::new(Mutex::new(engine)), None);
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
        let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let mcp = ForgeQlMcp::new(Arc::new(Mutex::new(engine)), None);
        let info = mcp.get_info();
        assert!(info.capabilities.tools.is_some());
    }

    #[test]
    fn get_info_has_instructions() {
        let tmp = tempdir().unwrap();
        let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let mcp = ForgeQlMcp::new(Arc::new(Mutex::new(engine)), None);
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
    fn run_fql_csv_format_returns_compact_output() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp.run_fql(Parameters(RunFqlParams {
            fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
            session_id: Some(session_id),
            format: Some(OutputFormat::Csv),
        }));
        let call_result = result.expect("should succeed");
        let text = first_text(&call_result);
        // Compact CSV: header row with op and total, schema hint, grouped data.
        assert!(
            text.contains("\"find_symbols\""),
            "compact output should have op in header: {text}"
        );
        assert!(
            text.contains("\"fql_kind\""),
            "compact output should have schema row: {text}"
        );
        assert!(
            text.contains("encenderMotor"),
            "compact output should contain symbol name: {text}"
        );
        assert!(
            text.contains("tokens_approx"),
            "compact output should have tokens_approx: {text}"
        );
    }
}
