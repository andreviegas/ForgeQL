/// `ForgeQL` MCP server handler.
///
/// Implements the Model Context Protocol (MCP) using `rmcp`.  Each `ForgeQL`
/// operation is exposed as an MCP tool.  The primary tool is `run_fql` which
/// accepts raw FQL and delegates to `ForgeQLEngine::execute()`.
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as TokioMutex;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ErrorData, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use tracing::{debug, error};

use forgeql_core::auth::{AuthContext, auth};
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
use forgeql_core::session::SessionCoords;

// -----------------------------------------------------------------------
// MCP handler struct
// -----------------------------------------------------------------------

/// MCP server handler wrapping a `ForgeQLEngine`.
///
/// The engine is behind an `Arc<TokioMutex>` because `ServerHandler` requires
/// `Sync` while `ForgeQLEngine::execute` takes `&mut self`.  Using
/// `tokio::sync::Mutex` ensures that a cancelled MCP request drops its
/// `.lock().await` waiter without blocking a thread — preventing the
/// all-threads-on-futex deadlock that occurs with `std::sync::Mutex` when
/// a long-running `USE` is cancelled by the client.
pub(crate) struct ForgeQlMcp {
    engine: Arc<TokioMutex<ForgeQLEngine>>,
    #[expect(
        dead_code,
        reason = "populated and read by the rmcp ToolRouter derive macro"
    )]
    tool_router: ToolRouter<Self>,
    logger: Mutex<Option<QueryLogger>>,
}

impl ForgeQlMcp {
    /// Create a new MCP handler wrapping the given engine.
    pub(crate) fn new(engine: Arc<TokioMutex<ForgeQLEngine>>, logger: Option<QueryLogger>) -> Self {
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
        budget_line: Option<&str>,
    ) {
        if let Ok(mut guard) = self.logger.lock()
            && let Some(ref mut l) = *guard
        {
            l.log(fql, result, output, elapsed_ms, source, budget_line);
        }
    }

    /// Resolve the source name for a session from the engine.
    async fn resolve_source(&self, session_id: Option<&str>) -> String {
        let Some(sid) = session_id else {
            return "unknown".to_string();
        };
        // session_id is already the full map key ({user}:{source}:{branch}:{alias}).
        self.engine
            .lock()
            .await
            .source_name_for_session(sid)
            .map_or_else(|| "unknown".to_string(), str::to_owned)
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
    /// The opaque session token returned by USE in the `session_id` field of the response.
    /// Required for all queries and mutations after the initial USE.
    /// Store the value exactly as returned and pass it verbatim — do not reconstruct it.
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
#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err requires taking ownership; the value cannot be passed by reference"
)]
fn parse_error(err: ForgeError) -> ErrorData {
    ErrorData::internal_error(format!("{err:#}"), None)
}

/// Convert an `anyhow::Error` (from the engine) to an `rmcp` `ErrorData`.
///
/// Takes ownership because `map_err` passes the error by value.
#[expect(
    clippy::needless_pass_by_value,
    reason = "map_err requires taking ownership; the value cannot be passed by reference"
)]
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
fn append_meta(output: &str, budget_line: Option<&str>) -> String {
    /// Approximate number of UTF-8 characters per LLM token.
    const CHARS_PER_TOKEN: usize = 4;
    /// Below this estimate the agent never needs to narrow the query, so the
    /// `tokens_approx` footer is pure noise and is omitted. `budget_line` is
    /// already `None` unless the session budget is in a warning/critical state
    /// (gated by the caller), so both footers now appear only when actionable.
    const TOKENS_FOOTER_THRESHOLD: usize = 500;

    let tokens_approx = output.len().div_ceil(CHARS_PER_TOKEN);
    let show_tokens = tokens_approx >= TOKENS_FOOTER_THRESHOLD;

    let budget_csv = budget_line
        .map(|b| format!("\n\"line_budget\",\"{b}\""))
        .unwrap_or_default();
    let tokens_csv = if show_tokens {
        format!("\n\"tokens_approx\",{tokens_approx}")
    } else {
        String::new()
    };
    let budget_json = budget_line
        .map(|b| format!(",\"line_budget\":\"{b}\""))
        .unwrap_or_default();
    let tokens_json = if show_tokens {
        format!(",\"tokens_approx\":{tokens_approx}")
    } else {
        String::new()
    };

    output.strip_suffix('}').map_or_else(
        // Compact CSV — append as final rows.
        || format!("{output}{budget_csv}{tokens_csv}"),
        // JSON object — splice fields before closing brace.
        |prefix| format!("{prefix}{budget_json}{tokens_json}}}"),
    )
}

/// Execute a parsed `ForgeQLIR` operation and return the raw result.
///
/// Callers are responsible for serializing to the desired output format.
///
/// # Cancellation safety
///
/// The engine is held under a `tokio::sync::Mutex`.  Acquiring the lock via
/// `.lock().await` is cancel-safe: if the MCP client drops the request while
/// this future is awaiting the lock, the waiter is simply removed from the
/// queue — no thread remains blocked.  A request that has already acquired
/// the lock and is executing inside `execute()` will always run to completion
/// (there is no mid-execution cancellation), but subsequent requests will not
/// pile up on a futex.
///
/// # Panic safety
///
/// The `execute()` call is wrapped in `catch_unwind`.  `tokio::sync::Mutex`
/// does not track poison state, so a panicking call simply releases the lock
/// normally after the unwind — subsequent requests can proceed.
async fn exec_engine(
    engine: &TokioMutex<ForgeQLEngine>,
    user_id: &str,
    session_id: Option<&str>,
    op: &ForgeQLIR,
) -> Result<
    (
        ForgeQLResult,
        Option<forgeql_core::budget::BudgetSnapshot>,
        Option<std::path::PathBuf>,
    ),
    ErrorData,
> {
    let mut guard = engine.lock().await;

    // Decode the opaque session token into full SessionCoords before entering
    // the engine.  The engine receives the struct directly — it never
    // reconstructs session identity from raw strings.
    let coords = session_id
        .map(|sid| {
            SessionCoords::from_session_id(sid)
                .map_err(|e| ErrorData::invalid_params(format!("invalid session_id: {e}"), None))
        })
        .transpose()?;

    // Wrap execute() in catch_unwind to convert panics into error responses.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        guard.execute(user_id, coords.as_ref(), op)
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

    // Query budget status while the lock is still held.
    // Use budget_status_for_op so admin commands (ShowBranches, ShowSources,
    // CreateSource, RefreshSource) produce no snapshot — they should not
    // appear in the budget log with stale delta values.
    // session_id is already the full map key ({user}:{source}:{branch}:{alias}).
    let budget_snap = session_id.and_then(|mk| guard.budget_status_for_op(mk, op));

    // Locate the session worktree while the lock is held — the CSV transport
    // uses it to write the SHOW MORE buffer for over-cap output.
    let worktree = session_id.and_then(|mk| guard.session_worktree(mk));

    drop(guard);
    Ok((result, budget_snap, worktree))
}

/// Decide whether a result's rendered CSV output should be buffered for
/// `SHOW MORE`, returning `(label, direction, inline-cap)` when so.
///
/// Enabled for `VERIFY build`, whose full log is the largest single output
/// sink. The buffer mechanism is general — other result types roll in here
/// without changing the call site.
fn buffering_params(
    result: &ForgeQLResult,
) -> Option<(String, forgeql_core::showmore::Direction, usize)> {
    use forgeql_core::config::SummaryDirection;
    use forgeql_core::showmore::Direction;
    match result {
        ForgeQLResult::VerifyBuild(v) => {
            let dir = match v.summary_direction {
                SummaryDirection::Tail => Direction::Tail,
                SummaryDirection::Head => Direction::Head,
            };
            Some((format!("verify_build '{}'", v.step), dir, v.summary_lines))
        }
        _ => None,
    }
}

/// Apply the `SHOW MORE` buffering to a rendered CSV output when the result
/// type opts in and exceeds its inline cap. Returns the (possibly windowed)
/// text to display; the full output is written to the session buffer.
fn finalize_csv(
    rendered: String,
    result: &ForgeQLResult,
    worktree: Option<&std::path::Path>,
) -> String {
    let Some(root) = worktree else {
        return rendered;
    };
    let Some((label, dir, cap)) = buffering_params(result) else {
        return rendered;
    };
    match forgeql_core::showmore::finalize(root, &rendered, &label, dir, cap) {
        Ok(fin) => fin.text,
        Err(_) => rendered,
    }
}
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
        description = "Execute any ForgeQL statement. CONNECT FIRST: USE source.branch AS 'alias' — the response returns an opaque session_id token; store it and pass it verbatim in ALL subsequent calls. OUTPUT: format defaults to CSV (pass format=JSON only when parsing fields programmatically). LIMIT: FIND queries without LIMIT default to 20 rows; add LIMIT N to override; when total > results.len() more rows exist. WORKFLOW: start narrow (WHERE/IN/LIMIT), verify, then widen."
    )]
    async fn run_fql(
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
        // user_id is the single birth point for identity in MCP mode.
        // When auth is implemented, replace this with a JWT / API-key lookup
        // on the incoming request — the lines below stay exactly as-is.
        let user_id = auth(AuthContext::Mcp);
        let format = params.format.unwrap_or_default();
        let mut log_source = self.resolve_source(params.session_id.as_deref()).await;
        let mut session_hint: Option<String> = None;
        let mut outputs: Vec<String> = Vec::with_capacity(ops.len());
        for (source_text, op) in &ops {
            let t0 = std::time::Instant::now();
            let (result, budget_snap, worktree) =
                exec_engine(&self.engine, user_id, params.session_id.as_deref(), op).await?;
            let elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
            if let ForgeQLIR::UseSource { source, .. } = op {
                log_source.clone_from(source);
            }
            // After auto-reconnect the session now exists — re-resolve the
            // source name so the query lands in the right CSV log file.
            if log_source == "unknown" {
                log_source = self.resolve_source(params.session_id.as_deref()).await;
            }
            // Extract session_id (alias) from USE responses and build a hint for the agent.
            if session_hint.is_none()
                && let ForgeQLResult::SourceOp(ref sop) = result
                && let Some(sid) = sop.session_id.as_deref()
            {
                session_hint = Some(format!(
                    "\u{26a0}\u{fe0f} IMPORTANT: Pass session_id \"{sid}\" in ALL subsequent run_fql calls. \
                     Store this token exactly as returned — do not reconstruct it from the alias."
                ));
            }
            let output = match format {
                OutputFormat::Csv => {
                    finalize_csv(compact::to_compact(&result), &result, worktree.as_deref())
                }
                OutputFormat::Json => result.to_json(),
            };
            // Show line_budget to the agent only when the budget is actually
            // low — an OK budget echoed on every response is noise (Phase 0).
            // budget_fixed below is unaffected: logging always records it.
            let budget_line = budget_snap
                .as_ref()
                .filter(|b| b.warning || b.critical)
                .map(forgeql_core::budget::BudgetSnapshot::status_line);
            let budget_fixed = budget_snap
                .as_ref()
                .map(forgeql_core::budget::BudgetSnapshot::fixed_status_line);
            let output = append_meta(&output, budget_line.as_deref());
            self.log_query(
                source_text,
                &result,
                &output,
                elapsed_ms,
                &log_source,
                budget_fixed.as_deref(),
            );
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
             - Always start with USE source.branch AS 'alias' before any query. \
               The alias you choose IS the session_id — pass it in every subsequent call. \
               Example: USE myrepo.main AS 'my-session' → session_id = \"my-session\".\n\
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
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use forgeql_core::ast::lang::LanguageRegistry;
    use forgeql_lang_c::CLanguage;
    use forgeql_lang_cpp::CppLanguage;
    use tempfile::tempdir;

    fn make_registry() -> Arc<LanguageRegistry> {
        Arc::new(LanguageRegistry::new(vec![
            Arc::new(CLanguage),
            Arc::new(CppLanguage),
        ]))
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
        // Register the session under the MCP auth user so run_fql can find it.
        let session_id = engine
            .register_local_session_for(auth(AuthContext::Mcp), dir.path())
            .expect("register session");
        let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);
        (mcp, session_id, dir)
    }

    fn first_text(result: &CallToolResult) -> &str {
        result.content[0]
            .as_text()
            .expect("expected text content")
            .text
            .as_str()
    }

    #[tokio::test]
    async fn get_info_returns_tools_capability() {
        let tmp = tempdir().unwrap();
        let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);
        let info = mcp.get_info();
        assert!(info.capabilities.tools.is_some());
    }

    #[tokio::test]
    async fn get_info_has_instructions() {
        let tmp = tempdir().unwrap();
        let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
        let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);
        let info = mcp.get_info();
        let instructions = info.instructions.expect("should have instructions");
        assert!(instructions.contains("ForgeQL"));
    }

    #[tokio::test]
    async fn run_fql_find_symbols() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp
            .run_fql(Parameters(RunFqlParams {
                fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
                session_id: Some(session_id),
                format: None,
            }))
            .await;
        let call_result = result.expect("should succeed");
        let text = first_text(&call_result);
        assert!(
            text.contains("encenderMotor"),
            "JSON should contain symbol: {text}"
        );
    }

    #[tokio::test]
    async fn run_fql_invalid_syntax_returns_error() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp
            .run_fql(Parameters(RunFqlParams {
                fql: "NOT VALID FQL".to_string(),
                session_id: Some(session_id),
                format: None,
            }))
            .await;
        assert!(result.is_err(), "invalid FQL should return ErrorData");
    }

    #[tokio::test]
    async fn run_fql_create_source_is_blocked() {
        let (mcp, _session_id, _dir) = mcp_with_session();
        let result = mcp
            .run_fql(Parameters(RunFqlParams {
                fql: "CREATE SOURCE 'evil' FROM 'https://example.com/repo.git'".to_string(),
                session_id: None,
                format: None,
            }))
            .await;
        assert!(result.is_err(), "CREATE SOURCE via MCP must be rejected");
        let err = result.unwrap_err();
        let msg = format!("{err:?}");
        assert!(
            msg.contains("not permitted") || msg.contains("administrator"),
            "error should mention admin restriction: {msg}"
        );
    }

    #[tokio::test]
    async fn run_fql_csv_format_returns_compact_output() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp
            .run_fql(Parameters(RunFqlParams {
                fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
                session_id: Some(session_id),
                format: Some(OutputFormat::Csv),
            }))
            .await;
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
        // tokens_approx is size-gated (Phase 0 noise reduction): a small result
        // omits the footer. This query returns a handful of rows, well under the
        // threshold, so the footer must be absent.
        assert!(
            !text.contains("tokens_approx"),
            "small compact output should omit the tokens_approx footer: {text}"
        );
    }

    #[test]
    fn append_meta_gates_footers_by_size() {
        // Small CSV output, no budget, below the token threshold → no footers.
        let small = super::append_meta("\"find_symbols\",0\n\"x\",1", None);
        assert!(
            !small.contains("tokens_approx"),
            "small output must omit tokens_approx: {small}"
        );
        assert!(
            !small.contains("line_budget"),
            "no budget configured must omit line_budget: {small}"
        );

        // Large CSV output (> threshold): tokens_approx is restored as a hint.
        let big = super::append_meta(&format!("\"find_symbols\",0\n{}", "x".repeat(4096)), None);
        assert!(
            big.contains("tokens_approx"),
            "large output must keep tokens_approx as a narrowing hint"
        );

        // A budget line (the caller only passes Some when warning/critical) is
        // shown regardless of size.
        let budgeted = super::append_meta("\"x\",1", Some("12 (-5)"));
        assert!(
            budgeted.contains("line_budget"),
            "a low-budget line must be shown regardless of size: {budgeted}"
        );
    }
}
