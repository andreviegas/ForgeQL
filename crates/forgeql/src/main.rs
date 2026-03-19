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

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, SourceOpResult};
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

    #[command(subcommand)]
    command: Option<Commands>,
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

    let engine = ForgeQLEngine::new(data_dir.clone())
        .with_context(|| format!("initialising engine with data_dir '{}'", data_dir.display()))?;

    let logger = cli.log_queries.then(|| QueryLogger::new(data_dir.clone()));

    match detect_mode(&cli) {
        Mode::Mcp => run_mcp_stdio(engine, logger).await,
        Mode::Repl => run_repl(engine, logger),
        Mode::Pipe => run_pipe(engine, logger),
        Mode::OneShot { fql, session } => {
            run_one_shot(engine, &fql, session.as_deref(), logger);
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

fn run_repl(mut engine: ForgeQLEngine, mut logger: Option<QueryLogger>) -> Result<()> {
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

                execute_and_print(&mut engine, trimmed, &mut session, logger.as_mut());
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

fn run_pipe(mut engine: ForgeQLEngine, mut logger: Option<QueryLogger>) -> Result<()> {
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

        execute_and_print(&mut engine, trimmed, &mut session, logger.as_mut());
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
) {
    let mut session = load_session_file();
    if let Some(sid) = session_override {
        session.session_id = Some(sid.to_string());
    }
    try_resume_session(&mut engine, &mut session);
    if let (Some(log), Some(src)) = (&mut logger, &session.source) {
        log.set_source(src);
    }

    execute_and_print(&mut engine, fql, &mut session, logger.as_mut());

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

    let ops = match parser::parse(&fql_text) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("parse error: {err}");
            return;
        }
    };

    // Consume the Option<&mut QueryLogger> as an ownable value so we can use
    // it across the loop without re-borrowing for each iteration.
    let mut log = logger;

    for op in &ops {
        match engine.execute(session.session_id.as_deref(), op) {
            Ok(result) => {
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
                }
                // Clear session on DISCONNECT.
                if matches!(op, ForgeQLIR::Disconnect) {
                    *session = SessionFile::default();
                }
                let output = format!("{result}");
                if let Some(ref mut l) = log {
                    l.log(&fql_text, &result, &output);
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
// Query logger
// -----------------------------------------------------------------------

/// Appends one CSV row per executed FQL statement to
/// `{data_dir}/log/{source}.csv`.
///
/// Enabled only when `--log-queries` is passed. The file is created (with a
/// header row) automatically on first write. All subsequent writes are
/// append-only so the log survives process restarts without duplication.
///
/// CSV columns:
/// `timestamp`, `command_preview`, `lines_returned`, `tokens_sent`, `tokens_received`
pub(crate) struct QueryLogger {
    data_dir: PathBuf,
    /// Sanitized source name — used as the CSV filename stem.
    source: String,
}

impl QueryLogger {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            source: "unknown".to_string(),
        }
    }

    /// Update the source name once a `USE source.branch` succeeds.
    pub(crate) fn set_source(&mut self, source: &str) {
        // Sanitize to safe filename characters.
        self.source = source
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
    }

    /// Append one CSV row for the completed FQL statement.
    ///
    /// `fql`           — the raw statement text.
    /// `result`        — the typed result, used to count disclosed source lines.
    /// `result_output` — the serialized output string, used to estimate token usage.
    pub(crate) fn log(&self, fql: &str, result: &ForgeQLResult, result_output: &str) {
        use std::io::Write;

        let log_dir = self.data_dir.join("log");
        if std::fs::create_dir_all(&log_dir).is_err() {
            return;
        }
        let log_path = log_dir.join(format!("{}.csv", self.source));

        let needs_header = !log_path.exists();
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        else {
            return;
        };
        if needs_header {
            let _ = writeln!(
                file,
                "timestamp,source_lines,tokens_sent,tokens_received,command_preview"
            );
        }

        // Clip command to 80 chars, flatten newlines, and CSV-escape quotes.
        let preview: String = fql
            .chars()
            .take(80)
            .collect::<String>()
            .replace(['\n', '\r', '\t'], " ")
            .replace('"', "\"\"");

        let source_lines = result.source_lines_count();
        // Token approximation: 1 token ≈ 4 UTF-8 characters.
        let tokens_sent = fql.len().div_ceil(4);
        let tokens_received = result_output.len().div_ceil(4);

        let _ = writeln!(
            file,
            r#""{}",{},{},{},"{}""#,
            iso_timestamp(),
            source_lines,
            tokens_sent,
            tokens_received,
            preview,
        );
    }
}

/// Return the current UTC time as an ISO 8601–style string (`YYYY-MM-DD HH:MM:SS`).
///
/// Uses only `std::time::SystemTime` — no external crates required.
fn iso_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, mo, d, h, mi, s) = epoch_to_datetime(secs);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}")
}

/// Decompose a Unix epoch timestamp (seconds since 1970-01-01 UTC) into
/// `(year, month, day, hour, minute, second)` using the proleptic Gregorian calendar.
///
/// Algorithm: <https://howardhinnant.github.io/date_algorithms.html>
#[allow(clippy::many_single_char_names)]
// All `as u32` casts in this function are safe: the values are bounded by
// modulo arithmetic (`% 60`, `% 60`, `% 24`) and Gregorian calendar limits,
// so none can exceed `u32::MAX` in practice.
#[allow(clippy::cast_possible_truncation)]
const fn epoch_to_datetime(secs: u64) -> (u32, u32, u32, u32, u32, u32) {
    let s = (secs % 60) as u32;
    let mi = ((secs / 60) % 60) as u32;
    let h = ((secs / 3_600) % 24) as u32;

    // Days since 1970-01-01; shift to days since 0000-03-01.
    let z = secs / 86_400 + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y0 = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y0 + 1 } else { y0 } as u32;

    (y, mo, d, h, mi, s)
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
        });
        assert_eq!(r.source_lines_count(), 0);
    }

    #[test]
    fn source_lines_count_zero_for_show_members() {
        let r = ForgeQLResult::Show(ShowResult {
            op: "show_members".to_string(),
            symbol: Some("MyClass".to_string()),
            file: None,
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
