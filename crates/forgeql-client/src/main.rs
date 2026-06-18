//! `forgeql-client` — thin terminal client for `forgeql-server`.
//!
//! Connects over HTTP and speaks the MCP JSON-RPC `run_fql` tool. Three modes:
//! an interactive REPL (default), a one-shot statement (`-e`), and a piped
//! script read from stdin. The `session_id` token the server returns from a
//! `USE` statement is captured automatically and threaded into later requests,
//! so `USE` followed by `FIND`/`SHOW` works across REPL lines and piped scripts.
#![allow(missing_docs)]

use std::cell::RefCell;
use std::io::{IsTerminal, Read};

use anyhow::{Context, Result, bail};
use clap::Parser;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use serde_json::{Value, json};

/// Command-line arguments for `forgeql-client`.
#[derive(Parser, Debug)]
#[command(name = "forgeql-client", about = "ForgeQL terminal client")]
struct Cli {
    /// Server host to connect to.
    #[arg(long, env = "FORGEQL_HOST", default_value = "localhost")]
    host: String,

    /// Server port to connect to.
    #[arg(long, env = "FORGEQL_PORT", default_value_t = 8080)]
    port: u16,

    /// Connect using TLS (HTTPS) instead of plain HTTP.
    #[arg(long)]
    tls: bool,

    /// Execute a single statement and exit (skips the REPL).
    #[arg(short = 'e', long)]
    execute: Option<String>,

    /// Output format requested from the server.
    #[arg(long, default_value = "CSV", value_parser = ["CSV", "JSON"])]
    format: String,

    /// Bearer token sent as `Authorization: Bearer <token>` for authenticated commands.
    #[arg(long, env = "FORGEQL_TOKEN")]
    token: Option<String>,
}

/// HTTP MCP client. Holds the base URL and the current session token.
struct Client {
    base_url: String,
    http: reqwest::blocking::Client,
    format: String,
    /// Bearer token sent on every request, if configured.
    token: Option<String>,
    session_id: RefCell<Option<String>>,
}

impl Client {
    fn new(base_url: String, format: String, token: Option<String>) -> Self {
        Self {
            base_url,
            http: reqwest::blocking::Client::new(),
            format,
            token,
            session_id: RefCell::new(None),
        }
    }

    /// Send one FQL statement and return its text output. Captures any
    /// server-issued `session_id` so the next call resumes the same session.
    fn run_fql(&self, fql: &str) -> Result<String> {
        let mut arguments = json!({ "fql": fql, "format": self.format });
        if let Some(sid) = self.session_id.borrow().as_ref() {
            arguments["session_id"] = Value::String(sid.clone());
        }
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "run_fql", "arguments": arguments },
        });

        let mut builder = self
            .http
            .post(format!("{}/mcp", self.base_url))
            .json(&request);
        if let Some(token) = self.token.as_ref() {
            builder = builder.bearer_auth(token);
        }
        let response: Value = builder
            .send()
            .with_context(|| format!("sending request to {}", self.base_url))?
            .json()
            .context("decoding server response")?;

        if let Some(err) = response.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("{msg}");
        }

        let result = response.get("result").cloned().unwrap_or(Value::Null);
        if let Some(sid) = result.get("session_id").and_then(Value::as_str) {
            *self.session_id.borrow_mut() = Some(sid.to_string());
        }

        let text = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Ok(text)
    }

    /// Probe `GET /health`; returns true if the server is reachable and healthy.
    fn healthy(&self) -> bool {
        self.http
            .get(format!("{}/health", self.base_url))
            .send()
            .is_ok_and(|r| r.status().is_success())
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let scheme = if cli.tls { "https" } else { "http" };
    let base_url = format!("{scheme}://{}:{}", cli.host, cli.port);
    let client = Client::new(base_url, cli.format.clone(), cli.token.clone());

    // One-shot mode: run a single statement and exit.
    if let Some(stmt) = cli.execute.as_deref() {
        println!("{}", client.run_fql(stmt)?);
        return Ok(());
    }

    // Pipe mode: stdin is not a terminal — execute each non-empty, non-comment
    // line. The session token persists across lines within this invocation.
    if !std::io::stdin().is_terminal() {
        let mut buf = String::new();
        let _ = std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading stdin")?;
        for line in buf.lines() {
            let stmt = line.trim();
            if stmt.is_empty() || stmt.starts_with('#') {
                continue;
            }
            match client.run_fql(stmt) {
                Ok(out) => println!("{out}"),
                Err(e) => eprintln!("error: {e}"),
            }
        }
        return Ok(());
    }

    run_repl(&client, &cli)
}

/// Interactive read-eval-print loop.
fn run_repl(client: &Client, cli: &Cli) -> Result<()> {
    println!(
        "forgeql-client → {} (type 'exit', 'quit', or Ctrl-D to leave)",
        client.base_url
    );
    if client.healthy() {
        println!("connected to {}:{}", cli.host, cli.port);
    } else {
        eprintln!(
            "warning: {}:{} did not answer /health — is forgeql-server running there?",
            cli.host, cli.port
        );
    }

    let mut editor = DefaultEditor::new().context("initialising line editor")?;
    loop {
        match editor.readline("fql> ") {
            Ok(line) => {
                let stmt = line.trim();
                if stmt.is_empty() {
                    continue;
                }
                if matches!(stmt, "exit" | "quit") {
                    break;
                }
                let _ = editor.add_history_entry(stmt);
                match client.run_fql(stmt) {
                    Ok(out) => println!("{out}"),
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(e) => return Err(e).context("reading input"),
        }
    }
    Ok(())
}
