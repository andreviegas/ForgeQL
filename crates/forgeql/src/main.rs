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

mod mcp;
mod path_utils;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::compact;
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::query_logger::QueryLogger;
use forgeql_core::result::{ForgeQLResult, SourceOpResult};
use forgeql_lang_cpp::CppLanguage;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde::{Deserialize, Serialize};
use tracing::info;

// -----------------------------------------------------------------------
// CLI definition
// -----------------------------------------------------------------------

/// `ForgeQL` — AST-aware code transformation.
#[derive(Parser, Debug)]
#[command(name = "forgeql", version, about)]
struct Cli {
    /// Root directory for bare repos and worktrees (created if absent).
    #[arg(short, long, default_value = "./data", env = "FORGEQL_DATA_DIR")]
    data_dir: PathBuf,

    /// Run as MCP server over stdio (for AI agents).
    #[arg(long)]
    mcp: bool,

    /// Write a CSV query-log to {data-dir}/log/{source}.csv.
    ///
    /// Each executed statement appends one row with timestamp, clipped command
    /// (first 80 chars), lines returned, approximate tokens sent and received.
    #[arg(long)]
    log_queries: bool,

    /// Increase verbosity (-v = debug, -vv = trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Output format: compact (default), text, json.
    #[arg(long, default_value = "compact", global = true)]
    format: CliFormat,

    #[command(subcommand)]
    command: Option<Commands>,
}

/// CLI output format.
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum CliFormat {
    /// Human-friendly terminal output.
    Text,
    /// Token-efficient compact CSV (default).
    #[default]
    Compact,
    /// Full structured JSON.
    Json,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Execute a `ForgeQL` statement and exit.
    Run {
        /// `ForgeQL` statement string (e.g. "FIND symbols WHERE name LIKE 'set%'").
        fql: String,
        /// Session ID from a previous USE command.
        #[arg(long, env = "FORGEQL_SESSION")]
        session: Option<String>,
    },
}

// -----------------------------------------------------------------------
// Mode detection
// -----------------------------------------------------------------------

enum Mode {
    Mcp,
    Repl,
    Pipe,
    OneShot {
        fql: String,
        session: Option<String>,
    },
}

fn detect_mode(cli: &Cli) -> Mode {
    if cli.mcp {
        return Mode::Mcp;
    }
    if let Some(Commands::Run {
        ref fql,
        ref session,
    }) = cli.command
    {
        return Mode::OneShot {
            fql: fql.clone(),
            session: session.clone(),
        };
    }
    if !std::io::stdin().is_terminal() {
        return Mode::Pipe;
    }
    Mode::Repl
}

// -----------------------------------------------------------------------
// Entry point
// -----------------------------------------------------------------------

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

    let lang_registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));

    let engine = ForgeQLEngine::new(data_dir.clone(), lang_registry)
        .with_context(|| format!("initialising engine with data_dir '{}'", data_dir.display()))?;

    let logger = cli.log_queries.then(|| QueryLogger::new(data_dir.clone()));

    match detect_mode(&cli) {
        Mode::Mcp => run_mcp_stdio(engine, logger).await,
        Mode::Repl => run_repl(engine, logger, cli.format),
        Mode::Pipe => run_pipe(engine, logger, cli.format),
        Mode::OneShot { fql, session } => {
            run_one_shot(engine, &fql, session.as_deref(), logger, cli.format);
            Ok(())
        }
    }
}

// -----------------------------------------------------------------------
// MCP mode — stdio transport
// -----------------------------------------------------------------------

async fn run_mcp_stdio(engine: ForgeQLEngine, logger: Option<QueryLogger>) -> Result<()> {
    use rmcp::ServiceExt;

    // MCP is a long-lived service — orphaned worktrees are truly abandoned.
    engine.prune_orphaned_worktrees();

    info!("starting MCP server over stdio");
    let handler = mcp::ForgeQlMcp::new(engine, logger);

    let service = handler
        .serve(rmcp::transport::io::stdio())
        .await
        .context("MCP service initialisation failed")?;

    // Block until the client disconnects.
    let _quit_reason = service.waiting().await?;
    info!("MCP session ended");
    Ok(())
}

// -----------------------------------------------------------------------
// REPL mode — interactive terminal
// -----------------------------------------------------------------------

fn run_repl(
    mut engine: ForgeQLEngine,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) -> Result<()> {
    let mut editor = DefaultEditor::new().context("failed to initialise line editor")?;

    // Load history from config dir.
    let history_path = session_dir().map(|d| d.join("history.txt"));
    if let Some(ref path) = history_path {
        let _ = editor.load_history(path);
    }

    // Try to resume a saved session.
    let mut session = load_session_file();
    try_resume_session(&mut engine, &mut session);
    // Seed the logger source from an already-saved session (resume path).
    if let (Some(log), Some(src)) = (&mut logger, &session.source) {
        log.set_source(src);
    }

    println!(
        "ForgeQL v{} — type 'help' or 'exit'",
        env!("CARGO_PKG_VERSION")
    );
    println!();

    loop {
        let prompt = session.session_id.as_ref().map_or_else(
            || "forgeql> ".to_string(),
            |sid| format!("forgeql [{sid}]> "),
        );

        match editor.readline(&prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(trimmed);

                match trimmed {
                    "exit" | "quit" | "\\q" => break,
                    "help" | "\\h" => {
                        print_repl_help();
                        continue;
                    }
                    _ => {}
                }

                execute_and_print(&mut engine, trimmed, &mut session, logger.as_mut(), format);
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("readline error: {err}");
                break;
            }
        }
    }

    // Save history and session.
    if let Some(ref path) = history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = editor.save_history(path);
    }
    save_session_file(&session);

    Ok(())
}

fn print_repl_help() {
    println!("  FIND symbols WHERE name LIKE 'set%'");
    println!("  FIND usages OF 'showCode'");
    println!("  FIND defines");
    println!("  FIND enums");
    println!("  SHOW body OF 'myFunction'");
    println!("  SHOW outline OF 'src/main.cpp'");
    println!("  RENAME symbol 'old' TO 'new'");
    println!("  CREATE SOURCE 'name' FROM 'url'");
    println!("  USE source.branch");
    println!("  exit / quit / \\q");
    println!();
}

// -----------------------------------------------------------------------
// Pipe mode — read from stdin, human output to stdout
// -----------------------------------------------------------------------

fn run_pipe(
    mut engine: ForgeQLEngine,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) -> Result<()> {
    use std::io::BufRead;

    let mut session = load_session_file();
    try_resume_session(&mut engine, &mut session);
    if let (Some(log), Some(src)) = (&mut logger, &session.source) {
        log.set_source(src);
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        execute_and_print(&mut engine, trimmed, &mut session, logger.as_mut(), format);
    }

    save_session_file(&session);
    Ok(())
}

// -----------------------------------------------------------------------
// One-shot mode — execute a single statement and exit
// -----------------------------------------------------------------------

fn run_one_shot(
    mut engine: ForgeQLEngine,
    fql: &str,
    session_override: Option<&str>,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) {
    let mut session = load_session_file();
    if let Some(sid) = session_override {
        session.session_id = Some(sid.to_string());
    }
    try_resume_session(&mut engine, &mut session);
    if let (Some(log), Some(src)) = (&mut logger, &session.source) {
        log.set_source(src);
    }

    execute_and_print(&mut engine, fql, &mut session, logger.as_mut(), format);

    save_session_file(&session);
}

// -----------------------------------------------------------------------
// Shared execution helpers
// -----------------------------------------------------------------------

/// Attempt to resume a saved session across CLI invocations.
///
/// Each CLI process starts with a fresh `ForgeQLEngine` (no in-memory sessions).
/// If the session file records a previous `session_id` plus the source/branch that
/// created it, we silently re-execute `USE source.branch` to restore the session.
/// On success the session gets a new id (the old worktree is reused); on failure
/// we clear the stale session so the user isn't stuck with a broken reference.
fn try_resume_session(engine: &mut ForgeQLEngine, session: &mut SessionFile) {
    let Some(ref old_sid) = session.session_id else {
        return;
    };
    let (Some(source), Some(branch)) = (&session.source, &session.branch) else {
        // No source/branch info — legacy session file; clear the stale id.
        info!("session file has no source/branch info — clearing stale session");
        session.session_id = None;
        return;
    };

    let use_op = ForgeQLIR::UseSource {
        source: source.clone(),
        branch: branch.clone(),
        as_branch: session.as_branch.clone(),
    };

    match engine.execute(None, &use_op) {
        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            session_id: Some(ref new_sid),
            ..
        })) => {
            info!(%old_sid, %new_sid, %source, %branch, "session resumed");
            session.session_id = Some(new_sid.clone());
        }
        Ok(_) => {
            info!("USE did not return a session — clearing stale session");
            session.session_id = None;
        }
        Err(err) => {
            info!(%err, "failed to resume session — clearing stale session");
            *session = SessionFile::default();
        }
    }
}

/// Parse FQL, execute against the engine, print the result.
///
/// Updates `session` state if the operation returns a session (e.g. USE).
/// Clears session on DISCONNECT.
fn execute_and_print(
    engine: &mut ForgeQLEngine,
    fql: &str,
    session: &mut SessionFile,
    logger: Option<&mut QueryLogger>,
    format: CliFormat,
) {
    // Check if it's an .fql file path.
    let fql_text = if std::path::Path::new(fql)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("fql"))
        && std::path::Path::new(fql).exists()
    {
        match std::fs::read_to_string(fql) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("error reading '{fql}': {err}");
                return;
            }
        }
    } else {
        fql.to_string()
    };

    let ops = match parser::parse_with_source(&fql_text) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("parse error: {err}");
            return;
        }
    };

    // Consume the Option<&mut QueryLogger> as an ownable value so we can use
    // it across the loop without re-borrowing for each iteration.
    let mut log = logger;

    for (source_text, op) in &ops {
        let t0 = std::time::Instant::now();
        match engine.execute(session.session_id.as_deref(), op) {
            Ok(result) => {
                let elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
                // Capture session info from USE results.
                if let ForgeQLResult::SourceOp(ref sop) = result {
                    if let Some(ref sid) = sop.session_id {
                        session.session_id = Some(sid.clone());
                        info!(%sid, "session started");
                    }
                    // Capture source/branch from the IR for auto-resume.
                    if let ForgeQLIR::UseSource {
                        source,
                        branch,
                        as_branch,
                    } = op
                    {
                        session.source = Some(source.clone());
                        session.branch = Some(branch.clone());
                        session.as_branch.clone_from(as_branch);
                        // Update the logger source name now that we know it.
                        if let Some(ref mut l) = log {
                            l.set_source(source);
                        }
                    }
                    // Update logger source name for CREATE SOURCE too.
                    if let ForgeQLIR::CreateSource { name, .. } = op
                        && let Some(ref mut l) = log
                    {
                        l.set_source(name);
                    }
                }
                // Clear session on DISCONNECT.
                if matches!(op, ForgeQLIR::Disconnect) {
                    *session = SessionFile::default();
                }
                let output = match format {
                    CliFormat::Text => format!("{result}"),
                    CliFormat::Compact => compact::to_compact(&result),
                    CliFormat::Json => result.to_json_pretty(),
                };
                if let Some(ref mut l) = log {
                    l.log(source_text, &result, &output, elapsed_ms);
                }
                println!("{output}");
            }
            Err(err) => {
                eprintln!("error: {err:#}");
            }
        }
    }
}

// -----------------------------------------------------------------------
// Session file persistence
// -----------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
struct SessionFile {
    session_id: Option<String>,
    /// Source name for auto-resume (e.g. "pisco-code").
    #[serde(default)]
    source: Option<String>,
    /// Branch name for auto-resume (e.g. "main").
    #[serde(default)]
    branch: Option<String>,
    /// Custom branch alias for auto-resume (from `USE … AS 'name'`).
    #[serde(default)]
    as_branch: Option<String>,
}

/// Return the `ForgeQL` config directory (~/.config/forgeql/).
fn session_dir() -> Option<PathBuf> {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_dir.join("forgeql"))
}

fn session_file_path() -> Option<PathBuf> {
    session_dir().map(|d| d.join("session.json"))
}

fn load_session_file() -> SessionFile {
    let Some(path) = session_file_path() else {
        return SessionFile::default();
    };
    let Ok(data) = std::fs::read_to_string(&path) else {
        return SessionFile::default();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_session_file(sf: &SessionFile) {
    let Some(path) = session_file_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(sf) {
        let _ = std::fs::write(&path, json);
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use forgeql_core::result::{
        ForgeQLResult, MemberEntry, QueryResult, ShowContent, ShowResult, SourceLine,
    };
    use std::path::PathBuf;

    fn show_lines_result(n: usize) -> ForgeQLResult {
        ForgeQLResult::Show(ShowResult {
            op: "show_lines".to_string(),
            symbol: None,
            file: Some(PathBuf::from("src/foo.cpp")),
            total_lines: None,
            hint: None,
            content: ShowContent::Lines {
                lines: (1..=n)
                    .map(|i| SourceLine {
                        line: i,
                        text: format!("line {i}"),
                        marker: None,
                    })
                    .collect(),
                byte_start: None,
                depth: None,
            },
            start_line: Some(1),
            end_line: Some(n),
        })
    }

    #[test]
    fn source_lines_count_show_lines() {
        assert_eq!(show_lines_result(70).source_lines_count(), 70);
    }

    #[test]
    fn source_lines_count_zero_for_query() {
        let r = ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results: vec![],
            total: 0,
            metric_hint: None,
        });
        assert_eq!(r.source_lines_count(), 0);
    }

    #[test]
    fn source_lines_count_zero_for_show_members() {
        let r = ForgeQLResult::Show(ShowResult {
            op: "show_members".to_string(),
            symbol: Some("MyClass".to_string()),
            file: None,
            total_lines: None,
            hint: None,
            content: ShowContent::Members {
                members: vec![MemberEntry {
                    kind: "field".to_string(),
                    text: "int x;".to_string(),
                    line: 1,
                }],
                byte_start: 0,
            },
            start_line: None,
            end_line: None,
        });
        assert_eq!(r.source_lines_count(), 0);
    }

    #[test]
    fn source_lines_count_increments_with_depth() {
        // Simulates SHOW BODY DEPTH 1 (10 lines) vs DEPTH 2 (13 lines).
        assert_eq!(show_lines_result(10).source_lines_count(), 10);
        assert_eq!(show_lines_result(13).source_lines_count(), 13);
    }
}
