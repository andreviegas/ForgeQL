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
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors QueryLogger::log: per-source selector, agent session id, and execution metrics"
    )]
    fn log_query(
        &self,
        fql: &str,
        result: &ForgeQLResult,
        output: &str,
        elapsed_ms: u64,
        source: &str,
        session_id: &str,
        budget_line: Option<&str>,
    ) {
        if let Ok(mut guard) = self.logger.lock()
            && let Some(ref mut l) = *guard
        {
            l.log(
                fql,
                result,
                output,
                elapsed_ms,
                source,
                session_id,
                budget_line,
            );
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

/// Return the error's message when it is itself a JSON object — the engine's
/// structured self-healing rejections (e.g. `rev_mismatch` carrying the
/// current rev, line range, and source; `node_not_found`).
///
/// Such payloads are results the agent is meant to parse and act on, so the
/// transport returns them as an error-flagged tool result instead of burying
/// the JSON inside a protocol-error string.
fn json_object_message(err: &ErrorData) -> Option<String> {
    let msg = err.message.trim();
    if !msg.starts_with('{') {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(msg)
        .ok()
        .filter(serde_json::Value::is_object)
        .map(|_| msg.to_owned())
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
    /// Above this estimate the response is large enough that the agent almost
    /// certainly wanted a narrower query — append a one-line hint with the
    /// narrowing tools. (Usage logs: single responses have reached 50k+
    /// tokens from an unbounded FIND files DEPTH walk.)
    const BIG_RESPONSE_HINT_THRESHOLD: usize = 2000;
    /// The narrowing hint appended past `BIG_RESPONSE_HINT_THRESHOLD`.
    const NARROW_HINT: &str = "large response — narrow with WHERE / IN / EXCLUDE, \
                               add LIMIT N (page with OFFSET), or aggregate with \
                               GROUP BY … ORDER BY … LIMIT";

    let tokens_approx = output.len().div_ceil(CHARS_PER_TOKEN);
    let show_tokens = tokens_approx >= TOKENS_FOOTER_THRESHOLD;
    let show_hint = tokens_approx >= BIG_RESPONSE_HINT_THRESHOLD;

    let budget_csv = budget_line
        .map(|b| format!("\n\"line_budget\",\"{b}\""))
        .unwrap_or_default();
    let tokens_csv = if show_tokens {
        format!("\n\"tokens_approx\",{tokens_approx}")
    } else {
        String::new()
    };
    let hint_csv = if show_hint {
        format!("\n\"hint\",\"{NARROW_HINT}\"")
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
    let hint_json = if show_hint {
        format!(",\"hint\":\"{NARROW_HINT}\"")
    } else {
        String::new()
    };

    output.strip_suffix('}').map_or_else(
        // Compact CSV — append as final rows.
        || format!("{output}{budget_csv}{tokens_csv}{hint_csv}"),
        // JSON object — splice fields before closing brace.
        |prefix| format!("{prefix}{budget_json}{tokens_json}{hint_json}}}"),
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
        usize,
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

    // Inline output cap (lines) for the session, read while the lock is held.
    // The CSV transport windows over-cap output to this many lines and buffers
    // the full text for `SHOW MORE`. No session → the configured default.
    let inline_cap = session_id.map_or_else(
        || forgeql_core::config::OutputConfig::default().show_lines,
        |mk| guard.session_inline_cap(mk),
    );

    drop(guard);
    Ok((result, budget_snap, worktree, inline_cap))
}

/// Decide whether a result's rendered CSV output should be buffered for
/// `SHOW MORE`, returning `(label, direction, inline-cap)` when so.
///
/// Enabled for every bulk-output result type: `SHOW` and `FIND` (read output,
/// capped at the session's inline limit) plus `VERIFY build` / `RUN` (command
/// logs, capped at their summary window). `finalize` only writes a buffer when
/// the rendered output exceeds the cap, so small results pass through inline.
fn buffering_params(
    result: &ForgeQLResult,
    inline_cap: usize,
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
        ForgeQLResult::Run(v) => {
            let dir = match v.summary_direction {
                SummaryDirection::Tail => Direction::Tail,
                SummaryDirection::Head => Direction::Head,
            };
            Some((format!("run '{}'", v.step), dir, v.summary_lines))
        }
        // Read-oriented bulk output: SHOW (source lines) and FIND (rows). Both
        // page top-down and share the session's inline cap. `finalize` only
        // writes a buffer when the rendered output actually exceeds the cap, so
        // small results pass through inline and unchanged.
        ForgeQLResult::Show(_) => Some(("show".to_string(), Direction::Head, inline_cap)),
        ForgeQLResult::Query(_) => Some(("find".to_string(), Direction::Head, inline_cap)),
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
    inline_cap: usize,
) -> String {
    let Some(root) = worktree else {
        return rendered;
    };
    let Some((label, dir, cap)) = buffering_params(result, inline_cap) else {
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
            let (result, budget_snap, worktree, inline_cap) =
                match exec_engine(&self.engine, user_id, params.session_id.as_deref(), op).await {
                    Ok(parts) => parts,
                    // Structured engine rejections are tool results the agent
                    // parses and acts on — return them error-flagged instead
                    // of wrapping the JSON inside a protocol error.
                    Err(e) => {
                        if let Some(payload) = json_object_message(&e) {
                            return Ok(CallToolResult::error(vec![Content::text(payload)]));
                        }
                        return Err(e);
                    }
                };
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
                OutputFormat::Csv => finalize_csv(
                    compact::to_compact(&result),
                    &result,
                    worktree.as_deref(),
                    inline_cap,
                ),
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
                params.session_id.as_deref().unwrap_or(""),
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
             - SHOW output beyond {DEFAULT_SHOW_LINE_LIMIT} source lines without an explicit \
               LIMIT is WINDOWED: you get the first {DEFAULT_SHOW_LINE_LIMIT} lines plus a \
               SHOW MORE footer for the rest. Prefer narrowing over paging: filter with \
               WHERE text MATCHES '…' / LIKE '…', or address the exact construct with \
               SHOW NODE '<node_id>' or SHOW body OF 'symbol'. Add LIMIT N only when you \
               consciously need more than {DEFAULT_SHOW_LINE_LIMIT} lines.\n\
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

    #[test]
    fn json_object_message_extracts_only_json_objects() {
        let json_err = ErrorData::internal_error(
            r#"{"error":"rev_mismatch","expected":"h1","actual":"h2"}"#.to_string(),
            None,
        );
        let payload = json_object_message(&json_err).expect("JSON object message");
        assert!(payload.contains("rev_mismatch"));

        let plain = ErrorData::internal_error("no session named 'x'".to_string(), None);
        assert!(json_object_message(&plain).is_none());

        let brace_but_not_json = ErrorData::internal_error("{not json".to_string(), None);
        assert!(json_object_message(&brace_but_not_json).is_none());
    }

    /// A rejected `IF REV` guard is a structured self-healing payload the
    /// agent parses — it must arrive as an error-flagged tool result, not as
    /// a protocol error with the JSON buried in the message string.
    #[tokio::test]
    async fn structured_rejection_returns_error_flagged_tool_result() {
        let (mcp, session_id, _dir) = mcp_with_session();
        let result = mcp
            .run_fql(Parameters(RunFqlParams {
                fql: "DELETE NODE 'nffffffffffff.0001' IF REV 'h0000000000000000'".to_string(),
                session_id: Some(session_id),
                format: None,
            }))
            .await
            .expect("a structured rejection must be a tool result, not a protocol error");
        assert_eq!(result.is_error, Some(true), "result must be error-flagged");
        let text = first_text(&result);
        let payload: serde_json::Value =
            serde_json::from_str(text).expect("payload must be parseable JSON");
        assert_eq!(
            payload["error"], "node_not_found",
            "payload should be the structured rejection: {text}"
        );
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

        // Large CSV output (> threshold): tokens_approx is restored as a hint,
        // but the narrowing hint only fires past the big-response threshold.
        let big = super::append_meta(&format!("\"find_symbols\",0\n{}", "x".repeat(4096)), None);
        assert!(
            big.contains("tokens_approx"),
            "large output must keep tokens_approx as a narrowing hint"
        );
        assert!(
            !big.contains("\"hint\""),
            "mid-size output must not carry the narrowing hint: {}",
            &big[big.len().saturating_sub(200)..]
        );

        // Very large output (> big-response threshold): the narrowing hint
        // row is appended.
        let huge = super::append_meta(&format!("\"find_files\",0\n{}", "x".repeat(9000)), None);
        assert!(
            huge.contains("\"hint\",\"large response"),
            "huge output must carry the narrowing hint: {}",
            &huge[huge.len().saturating_sub(300)..]
        );

        // A budget line (the caller only passes Some when warning/critical) is
        // shown regardless of size.
        let budgeted = super::append_meta("\"x\",1", Some("12 (-5)"));
        assert!(
            budgeted.contains("line_budget"),
            "a low-budget line must be shown regardless of size: {budgeted}"
        );
    }

    #[test]
    fn show_and_query_route_through_show_more_buffer() {
        use forgeql_core::result::{QueryResult, ShowContent, ShowResult, SourceLine};

        // FIND (Query) opts into buffering so large result sets page via SHOW MORE.
        let query = ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results: vec![],
            total: 0,
            metric_hint: None,
            group_by_field: None,
            hint: None,
        });
        assert!(
            buffering_params(&query, 40).is_some(),
            "FIND output must route through the SHOW MORE buffer"
        );

        // A SHOW result with more lines than the cap is windowed inline while the
        // full rendered output is written to the session buffer for SHOW MORE,
        // replacing the old hard block that returned zero lines.
        let lines: Vec<SourceLine> = (1..=100)
            .map(|i| SourceLine {
                line: i,
                text: format!("source line {i}"),
                marker: None,
                node_id: None,
                node_offset: None,
            })
            .collect();
        let show = ForgeQLResult::Show(ShowResult {
            op: "show_lines".to_string(),
            symbol: None,
            file: Some(PathBuf::from("big.rs")),
            start_line: Some(1),
            end_line: Some(100),
            total_lines: None,
            hint: None,
            metadata: None,
            content: ShowContent::Lines {
                lines,
                byte_start: None,
                depth: None,
            },
        });
        assert!(
            buffering_params(&show, 40).is_some(),
            "SHOW output must route through the SHOW MORE buffer"
        );

        let tmp = tempdir().expect("tempdir");
        let rendered = compact::to_compact(&show);
        let full_lines = rendered.lines().count();
        assert!(full_lines > 40, "fixture must exceed the cap: {full_lines}");

        let windowed = finalize_csv(rendered, &show, Some(tmp.path()), 40);
        let shown = windowed.lines().count();
        assert!(
            shown <= 42,
            "over-cap SHOW must be windowed near the cap, got {shown} lines"
        );
        assert!(
            windowed.contains("show_more"),
            "windowed output must carry the SHOW MORE hint: {windowed}"
        );

        // The full output is recoverable from the session buffer.
        let buffer = forgeql_core::showmore::read_buffer(tmp.path())
            .expect("read buffer")
            .expect("buffer must exist after an over-cap SHOW");
        assert!(
            buffer.total() >= full_lines,
            "buffer must hold the full rendered output"
        );
    }

    /// Automotive structured-XML end-to-end through the MCP tool: AUTOSAR
    /// ECUC parameter values and tresos datamodel entries are findable by
    /// their real names — the discovery half of the workflow that replaces
    /// GUI round-trips through vendor configuration tools.
    #[tokio::test]
    async fn run_fql_automotive_xml_find_by_real_names() {
        use forgeql_lang_text::xml::XmlLanguage;

        let dir = tempdir().expect("tempdir");
        let src = fixtures_dir();
        for fixture in ["EcucCanIf.arxml", "TresosAdc.xdm"] {
            let _ = fs::copy(src.join(fixture), dir.path().join(fixture)).expect("copy fixture");
        }
        let registry = Arc::new(LanguageRegistry::new(vec![
            Arc::new(CLanguage),
            Arc::new(CppLanguage),
            Arc::new(XmlLanguage),
        ]));
        let mut engine = ForgeQLEngine::new(dir.path().join("data"), registry).expect("engine");
        let session_id = engine
            .register_local_session_for(auth(AuthContext::Mcp), dir.path())
            .expect("register session");
        let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);

        let run = |fql: &str, sid: String| {
            mcp.run_fql(Parameters(RunFqlParams {
                fql: fql.to_string(),
                session_id: Some(sid),
                format: None,
            }))
        };

        // An ECUC parameter value carries no SHORT-NAME and no identifying
        // attribute; it must be findable by its DEFINITION-REF's last path
        // segment. (Node-handle mutation on XML elements is covered by the
        // golden node-mutation suites against the git-backed corpora.)
        let result = run(
            "FIND symbols WHERE name = 'CanIfPublicTxBuffering'",
            session_id.clone(),
        )
        .await
        .expect("find param should succeed");
        let text = first_text(&result);
        assert!(
            text.contains("CanIfPublicTxBuffering") && text.contains("EcucCanIf.arxml"),
            "ECUC param must be findable by name: {text}"
        );

        // A deeply nested sub-container keeps its SHORT-NAME identity.
        let result = run(
            "FIND symbols WHERE name = 'CanIfBufferCfg_0'",
            session_id.clone(),
        )
        .await
        .expect("find container should succeed");
        let text = first_text(&result);
        assert!(
            text.contains("CanIfBufferCfg_0"),
            "nested container findable by SHORT-NAME: {text}"
        );

        // A tresos datamodel variable is named by its `name` attribute, deep
        // inside namespaced (`d:ctr`/`d:var`) nesting.
        let result = run("FIND symbols WHERE name = 'AdcPrescale'", session_id)
            .await
            .expect("find tresos var should succeed");
        let text = first_text(&result);
        assert!(text.contains("AdcPrescale"), "tresos var findable: {text}");
        assert!(text.contains("TresosAdc.xdm"), "path present: {text}");
    }
}
