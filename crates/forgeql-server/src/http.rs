//! HTTP transport for `forgeql-server`.
//!
//! `GET /health` (liveness) and `POST /mcp` (MCP JSON-RPC over streamable
//! HTTP). The endpoint implements the client-to-server half of the MCP
//! handshake — `initialize`, `notifications/*` (acknowledged with `202
//! Accepted`), `tools/list`, `ping` — plus `tools/call` for the `run_fql`
//! tool, so MCP clients such as Claude Code can connect to it directly as a
//! remote HTTP server. The engine is reached through the same path the stdio
//! MCP handler uses: parse → execute → compact-CSV/JSON render. When a
//! statement opens a session (`USE`), the engine-issued `session_id` token is
//! returned in the result envelope and in the response text so clients can
//! thread it into later calls. Server-initiated streaming (SSE) is not
//! implemented; `GET /mcp` answers `405 Method Not Allowed`, which
//! streamable-HTTP clients treat as "no server stream".

use std::sync::Arc;

use crate::auth::{Principal, TokenStore};
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use forgeql_core::compact;
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::query_logger::QueryLogger;
use forgeql_core::result::ForgeQLResult;
use forgeql_core::session::SessionCoords;
use serde_json::{Value, json};
use tokio::sync::Mutex as TokioMutex;
use tracing::debug;

/// Shared server state handed to every request handler.
#[derive(Clone)]
pub(crate) struct AppState {
    /// The `ForgeQL` engine. Behind an async mutex because `execute` takes
    /// `&mut self` while axum handlers run concurrently.
    pub(crate) engine: Arc<TokioMutex<ForgeQLEngine>>,
    /// Bearer-token to principal lookup used to authorise each request.
    pub(crate) auth: Arc<TokenStore>,
    /// CSV query logger (`--log-queries`); one row per executed statement.
    pub(crate) query_logger: Option<Arc<QueryLogger>>,
}

/// Build the application router.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/mcp", post(mcp_post))
        .with_state(state)
}

/// Liveness probe for Docker / Kubernetes.
async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Handle one MCP JSON-RPC request.
///
/// Dispatches the MCP lifecycle methods (`initialize`, `notifications/*`,
/// `tools/list`, `ping`) and `tools/call` for the `run_fql` tool. Unknown
/// methods and tools return a JSON-RPC error.
async fn mcp_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> Response {
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();

    // Notifications carry no id and expect no body — acknowledge with 202.
    if method.starts_with("notifications/") {
        return StatusCode::ACCEPTED.into_response();
    }

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let reply = match method {
        "initialize" => initialize_result(&id, &req),
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        "tools/list" => tools_list_result(&id),
        "tools/call" => tools_call(&state, &headers, &id, &req).await,
        other => rpc_error(&id, -32601, &format!("method not supported: {other}")),
    };
    Json(reply).into_response()
}

/// Fixed protocol revision this server speaks by default. Known revisions the
/// client asks for are echoed back; anything else falls back to this one, per
/// the MCP version-negotiation rules.
const PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol revisions this server accepts from a client. The tool surface is
/// identical across them, so echoing the client's choice is always safe.
const KNOWN_PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// Instructions surfaced to MCP clients at `initialize` — a condensed version
/// of the stdio server's guidance plus the multi-tenant specifics (opaque
/// `session_id` token, admin-gated source management).
const SERVER_INSTRUCTIONS: &str = "ForgeQL — AST-aware code transformation server. \
    All source code is accessed EXCLUSIVELY through ForgeQL queries via the run_fql tool.\n\
    - Always start with USE source.branch AS 'alias'. The response contains a session_id \
    token — store it and pass it verbatim in every subsequent run_fql call.\n\
    - Locate code with FIND symbols WHERE name LIKE '...' and read it with \
    SHOW body OF 'name' DEPTH N; never scan files by line ranges.\n\
    - Never fall back to local filesystem tools (grep, find, cat); ForgeQL manages \
    all code access and the local workspace may be empty.\n\
    - CREATE SOURCE, REFRESH SOURCE, and VACUUM require an admin token.";

/// Tool description shown in `tools/list`; mirrors the stdio server's
/// `run_fql` tool so agents get identical guidance on both transports.
const RUN_FQL_DESCRIPTION: &str = "Execute any ForgeQL statement. CONNECT FIRST: \
    USE source.branch AS 'alias' — the response returns an opaque session_id token; \
    store it and pass it verbatim in ALL subsequent calls. OUTPUT: format defaults to \
    CSV (pass format=JSON only when parsing fields programmatically). LIMIT: FIND \
    queries without LIMIT default to 20 rows; add LIMIT N to override; when total > \
    results.len() more rows exist. WORKFLOW: start narrow (WHERE/IN/LIMIT), verify, then widen.";

/// Build the `initialize` result: negotiated protocol version, tools
/// capability, server identity, and the connect-time instructions.
fn initialize_result(id: &Value, req: &Value) -> Value {
    let requested = req
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(PROTOCOL_VERSION);
    let version = if KNOWN_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSION
    };
    json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "result": {
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "forgeql-server",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": SERVER_INSTRUCTIONS,
        },
    })
}

/// Build the `tools/list` result: the single `run_fql` tool with an input
/// schema matching the stdio MCP server's.
fn tools_list_result(id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "result": {
            "tools": [{
                "name": "run_fql",
                "description": RUN_FQL_DESCRIPTION,
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "fql": {
                            "type": "string",
                            "description": "The ForgeQL statement to execute (e.g. \"FIND symbols WHERE name LIKE 'set%'\").",
                        },
                        "session_id": {
                            "type": ["string", "null"],
                            "description": "The opaque session token returned by USE in the response. Required for all queries and mutations after the initial USE. Pass it verbatim — do not reconstruct it.",
                        },
                        "format": {
                            "type": ["string", "null"],
                            "enum": ["CSV", "JSON", null],
                            "description": "Output format: \"CSV\" (default, compact grouped CSV) or \"JSON\" (full structured).",
                        },
                    },
                    "required": ["fql"],
                },
            }],
        },
    })
}

/// Handle a `tools/call` request for the `run_fql` tool.
async fn tools_call(state: &AppState, headers: &HeaderMap, id: &Value, req: &Value) -> Value {
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if tool != "run_fql" {
        return rpc_error(id, -32602, &format!("unknown tool: {tool}"));
    }

    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let Some(fql) = args.get("fql").and_then(Value::as_str) else {
        return rpc_error(id, -32602, "missing required argument: fql");
    };
    let session_id = args.get("session_id").and_then(Value::as_str);
    let format = args.get("format").and_then(Value::as_str).unwrap_or("CSV");

    let principal = state.auth.resolve(bearer_token(headers).as_deref());
    debug!(%fql, ?session_id, %format, user = %principal.user, "run_fql");

    match execute_fql(state, &principal, fql, session_id, format).await {
        Ok((text, new_session)) => {
            let mut result = json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            });
            if let Some(sid) = new_session {
                result["session_id"] = Value::String(sid);
            }
            json!({ "jsonrpc": "2.0", "id": id.clone(), "result": result })
        }
        Err(msg) => rpc_error(id, -32603, &msg),
    }
}

/// Extract a bearer token from the `Authorization` header, if present.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?
        .trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// Build a JSON-RPC 2.0 error response.
fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.clone(),
        "error": { "code": code, "message": message },
    })
}

/// Parse and execute one or more FQL statements.
///
/// Returns the concatenated output and, if any statement opened a session, the
/// engine-issued `session_id` token. Mirrors the stdio handler's `run_fql`:
/// each statement is executed individually. `CREATE SOURCE` and `REFRESH SOURCE`
/// are rejected — sources are administrator-managed.
async fn execute_fql(
    state: &AppState,
    principal: &Principal,
    fql: &str,
    session_id: Option<&str>,
    format: &str,
) -> Result<(String, Option<String>), String> {
    let ops = parser::parse_with_source(fql).map_err(|e| format!("parse error: {e}"))?;
    if ops.is_empty() {
        return Err("empty FQL statement".to_string());
    }

    // Source-management commands are reserved for admin principals. Normal and
    // anonymous callers may only connect to and query existing sources.
    if !principal.is_admin() {
        for (_, op) in &ops {
            if matches!(
                op,
                ForgeQLIR::CreateSource { .. }
                    | ForgeQLIR::RefreshSource { .. }
                    | ForgeQLIR::Vacuum { .. }
            ) {
                return Err(
                    "CREATE SOURCE, REFRESH SOURCE, and VACUUM require an admin token; \
                        use USE to connect to an existing source"
                        .to_string(),
                );
            }
        }
    }

    let user_id = principal.user.as_str();
    let coords = session_id
        .map(SessionCoords::from_session_id)
        .transpose()
        .map_err(|e| format!("invalid session_id: {e}"))?;

    let mut outputs = Vec::with_capacity(ops.len());
    let mut new_session: Option<String> = None;
    let mut guard = state.engine.lock().await;
    for (text, op) in &ops {
        let started = std::time::Instant::now();
        let result = guard
            .execute(user_id, coords.as_ref(), op)
            .map_err(|e| e.to_string())?;
        if let ForgeQLResult::SourceOp(sop) = &result
            && let Some(sid) = sop.session_id.as_deref()
        {
            new_session = Some(sid.to_string());
        }
        let rendered = if format.eq_ignore_ascii_case("json") {
            result.to_json()
        } else {
            compact::to_compact(&result)
        };
        // One log row per executed statement, mirroring the stdio MCP handler.
        // A USE earlier in the batch updates the session the row is keyed to.
        if let Some(logger) = state.query_logger.as_ref() {
            let sid = new_session.as_deref().or(session_id).unwrap_or("");
            let source = guard
                .source_name_for_session(sid)
                .map_or_else(|| "unknown".to_string(), str::to_owned);
            let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            logger.log(text, &result, &rendered, elapsed, &source, sid, None);
        }
        outputs.push(rendered);
    }
    drop(guard);

    Ok((outputs.join("\n"), new_session))
}

#[cfg(test)]
mod tests {
    #![expect(clippy::unwrap_used, reason = "test code")]
    use super::*;

    #[test]
    fn initialize_echoes_known_protocol_version() {
        let req = json!({ "params": { "protocolVersion": "2025-03-26" } });
        let resp = initialize_result(&Value::from(1), &req);
        assert_eq!(
            resp.pointer("/result/protocolVersion").unwrap(),
            "2025-03-26"
        );
        assert!(resp.pointer("/result/capabilities/tools").is_some());
        assert_eq!(
            resp.pointer("/result/serverInfo/name").unwrap(),
            "forgeql-server"
        );
    }

    #[test]
    fn initialize_falls_back_on_unknown_version() {
        let req = json!({ "params": { "protocolVersion": "1999-01-01" } });
        let resp = initialize_result(&Value::from(1), &req);
        assert_eq!(
            resp.pointer("/result/protocolVersion").unwrap(),
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn initialize_without_params_uses_default_version() {
        let resp = initialize_result(&Value::Null, &json!({}));
        assert_eq!(
            resp.pointer("/result/protocolVersion").unwrap(),
            PROTOCOL_VERSION
        );
    }

    #[test]
    fn tools_list_exposes_run_fql() {
        let resp = tools_list_result(&Value::from(7));
        assert_eq!(resp["id"], 7);
        assert_eq!(resp.pointer("/result/tools/0/name").unwrap(), "run_fql");
        assert_eq!(
            resp.pointer("/result/tools/0/inputSchema/required/0")
                .unwrap(),
            "fql"
        );
    }
}
