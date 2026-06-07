//! `forgeql-server` — multi-tenant MCP daemon.
//!
//! Increment 2: serves `GET /health` and `POST /mcp` (MCP `run_fql`, no auth)
//! over HTTP, backed by the shared `forgeql-core` engine. Authentication
//! (JWT / API keys) and the per-user session registry follow in later
//! increments.
#![allow(missing_docs)]
// In a binary crate, pub(crate) on items used across modules is the right
// visibility; clippy's redundant_pub_crate would otherwise fight unreachable_pub.
#![allow(clippy::redundant_pub_crate)]

mod http;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::engine::ForgeQLEngine;
use forgeql_lang_c::CLanguage;
use forgeql_lang_cpp::CppLanguage;
use forgeql_lang_markdown::MarkdownLanguage;
use forgeql_lang_python::PythonLanguage;
use forgeql_lang_rust::RustLanguage;
use tokio::sync::Mutex as TokioMutex;
use tracing::info;

use crate::http::AppState;

/// Command-line arguments for `forgeql-server`.
#[derive(Parser, Debug)]
#[command(name = "forgeql-server", about = "ForgeQL multi-tenant MCP daemon")]
struct Cli {
    /// Address to bind the HTTP listener to.
    #[arg(long, default_value = "0.0.0.0")]
    host: String,

    /// Port to bind the HTTP listener to.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Data directory for repos, worktrees, and indexes.
    #[arg(long)]
    data_dir: Option<std::path::PathBuf>,

    /// Verbosity: -v info, -vv debug, -vvv trace.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = match cli.verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| log_level.into()),
        )
        .init();

    let data_dir = cli
        .data_dir
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    let lang_registry = Arc::new(LanguageRegistry::new(vec![
        Arc::new(CLanguage),
        Arc::new(CppLanguage),
        Arc::new(MarkdownLanguage),
        Arc::new(PythonLanguage),
        Arc::new(RustLanguage),
    ]));

    let engine = ForgeQLEngine::new(data_dir.clone(), lang_registry)
        .with_context(|| format!("initialising engine with data_dir '{}'", data_dir.display()))?;
    let state = AppState {
        engine: Arc::new(TokioMutex::new(engine)),
    };

    let app = http::router(state);

    let addr = format!("{}:{}", cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding to {addr}"))?;
    info!(
        %addr,
        data_dir = %data_dir.display(),
        "forgeql-server listening — POST /mcp, GET /health (no auth yet)"
    );

    axum::serve(listener, app).await.context("server error")?;
    Ok(())
}
