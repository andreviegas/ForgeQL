//! MCP-over-stdio runner.

use std::sync::Arc;

use anyhow::{Context, Result};
use tracing::info;

use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::query_logger::QueryLogger;

use crate::mcp;

/// Start the MCP server, serving requests over stdin/stdout.
///
/// Prunes stale worktrees first (orphaned leftovers from previous runs),
/// then wraps the engine in `Arc<Mutex>` so it can be shared with the
/// background session-eviction task.
pub(crate) async fn run_mcp_stdio(
    engine: ForgeQLEngine,
    logger: Option<QueryLogger>,
) -> Result<()> {
    use forgeql_core::engine::SESSION_TTL_SECS;
    use rmcp::ServiceExt;
    use std::sync::Mutex;

    // Prune orphaned worktrees that were abandoned by previous MCP sessions.
    engine.prune_orphaned_worktrees();

    let engine = Arc::new(Mutex::new(engine));

    info!("starting MCP server over stdio");
    let handler = mcp::ForgeQlMcp::new(Arc::clone(&engine), logger);

    // Background task: evict idle sessions every 5 minutes.
    let eviction_handle = Arc::clone(&engine);
    let _eviction_task = tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(SESSION_TTL_SECS.min(300));
        loop {
            tokio::time::sleep(interval).await;
            if let Ok(mut eng) = eviction_handle.lock() {
                eng.evict_idle_sessions();
            }
        }
    });

    let service = handler
        .serve(rmcp::transport::io::stdio())
        .await
        .context("MCP service initialisation failed")?;

    // Block until the client disconnects.
    let _quit_reason = service.waiting().await?;
    info!("MCP session ended");
    Ok(())
}
