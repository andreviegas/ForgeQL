//! `ForgeQL` — AST-aware code transformation.
//!
//! Single binary with four auto-detected modes:
//!
//! | Invocation                   | Mode      |
//! |------------------------------|-----------|
//! | `forgeql` (TTY, no args)     | REPL      |
//! | `echo '...' | forgeql`       | Pipe      |
//! | `forgeql run 'FIND ...'`     | One-shot  |
//! | `forgeql --mcp`              | MCP stdio |

// TODO: add crate-level documentation before 1.0.
#![allow(missing_docs)]
// In a binary crate, pub(crate) vs pub inside non-pub modules is ambiguous.
#![allow(clippy::redundant_pub_crate)]

// Replace glibc malloc with jemalloc.  jemalloc's background decay thread
// returns freed pages to the OS — critical for recovering RSS after large
// index frees (e.g. ROLLBACK drop_index + resume_index on zephyr-scale
// sessions where the working set is >4 GB).
// Not enabled on Windows: jemalloc does not support MinGW cross-compilation.
#[cfg(not(windows))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod cli;
mod execute;
mod mcp;
mod path_utils;
mod runner;
mod session;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::query_logger::QueryLogger;
use forgeql_lang_cpp::CppLanguage;
use forgeql_lang_python::PythonLanguage;
use forgeql_lang_rust::RustLanguage;

use cli::{Cli, Mode, detect_mode};

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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| log_level.into()),
        )
        .init();

    let data_dir = path_utils::resolve_data_dir(&cli.data_dir);

    let lang_registry = Arc::new(LanguageRegistry::new(vec![
        Arc::new(CppLanguage),
        Arc::new(PythonLanguage),
        Arc::new(RustLanguage),
    ]));

    let engine = ForgeQLEngine::new(data_dir.clone(), lang_registry)
        .with_context(|| format!("initialising engine with data_dir '{}'", data_dir.display()))?;

    let logger = cli.log_queries.then(|| QueryLogger::new(data_dir.clone()));

    match detect_mode(&cli) {
        Mode::Mcp => runner::mcp_stdio::run_mcp_stdio(engine, logger).await,
        Mode::Repl => runner::repl::run_repl(engine, logger, cli.format),
        Mode::Pipe => runner::pipe::run_pipe(engine, logger, cli.format),
        Mode::OneShot { fql, session } => {
            runner::one_shot::run_one_shot(engine, &fql, session.as_deref(), logger, cli.format);
            Ok(())
        }
    }
}
