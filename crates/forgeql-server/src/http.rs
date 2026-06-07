//! HTTP transport for `forgeql-server`.
//!
//! `GET /health` (liveness) and `POST /mcp` (MCP JSON-RPC `tools/call` for the
//! `run_fql` tool, no auth yet). The engine is reached through the same path the
//! stdio MCP handler uses: parse → execute → compact-CSV/JSON render. When a
//! statement opens a session (`USE`), the engine-issued `session_id` token is
//! returned in the JSON-RPC result so the client can thread it into later calls.
//! Auth, streaming (SSE), and the session registry arrive in later increments.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::{get, post};
use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::compact;
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
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
/// Supports `tools/call` for the `run_fql` tool only. Any other method or tool
/// returns a JSON-RPC error.
async fn mcp_post(State(state): State<AppState>, Json(req): Json<Value>) -> Json<Value> {
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if method != "tools/call" {
        return Json(rpc_error(
            &id,
            -32601,
            &format!("method not supported: {method}"),
        ));
    }

    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let tool = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if tool != "run_fql" {
        return Json(rpc_error(&id, -32602, &format!("unknown tool: {tool}")));
    }

    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    let Some(fql) = args.get("fql").and_then(Value::as_str) else {
        return Json(rpc_error(&id, -32602, "missing required argument: fql"));
    };
    let session_id = args.get("session_id").and_then(Value::as_str);
    let format = args.get("format").and_then(Value::as_str).unwrap_or("CSV");

    debug!(%fql, ?session_id, %format, "run_fql");

    match execute_fql(&state.engine, fql, session_id, format).await {
        Ok((text, new_session)) => {
            let mut result = json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            });
            if let Some(sid) = new_session {
                result["session_id"] = Value::String(sid);
            }
            Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
        }
        Err(msg) => Json(rpc_error(&id, -32603, &msg)),
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
    engine: &TokioMutex<ForgeQLEngine>,
    fql: &str,
    session_id: Option<&str>,
    format: &str,
) -> Result<(String, Option<String>), String> {
    let ops = parser::parse_with_source(fql).map_err(|e| format!("parse error: {e}"))?;
    if ops.is_empty() {
        return Err("empty FQL statement".to_string());
    }

    for (_, op) in &ops {
        if matches!(
            op,
            ForgeQLIR::CreateSource { .. } | ForgeQLIR::RefreshSource { .. }
        ) {
            return Err(
                "CREATE SOURCE and REFRESH SOURCE are not permitted via MCP; \
                        use USE to connect to an existing source"
                    .to_string(),
            );
        }
    }

    let user_id = auth(AuthContext::Mcp);
    let coords = session_id
        .map(SessionCoords::from_session_id)
        .transpose()
        .map_err(|e| format!("invalid session_id: {e}"))?;

    let mut outputs = Vec::with_capacity(ops.len());
    let mut new_session: Option<String> = None;
    let mut guard = engine.lock().await;
    for (_, op) in &ops {
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
        outputs.push(rendered);
    }
    drop(guard);

    Ok((outputs.join("\n"), new_session))
}
