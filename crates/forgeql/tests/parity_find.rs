//! Phase 05 — `parity_find` real-workspace gate test (via MCP server).
//!
//! Spawns a `forgeql --mcp --log-queries --data-dir <dir>` subprocess and
//! talks JSON-RPC over stdio — the **same** transport agents and IDEs use.
//! Inside that server, a session is opened by sending:
//!
//! ```text
//! USE <source>.<branch> AS 'parity'
//! ```
//!
//! through the `run_fql` MCP tool, which triggers `build_index` + columnar
//! shadow-write + overlay build, installing **both** legacy and columnar
//! backends from the same index pass.
//!
//! Every corpus query is then executed twice through the same session:
//!   - **Legacy**:   `FIND symbols [clauses]`
//!   - **Columnar**: `FIND symbols USING 'columnar' [clauses]`
//!
//! Results are canonicalised by sorting on `(name, kind, line)` before
//! comparison — ORDER BY differences do not cause false failures; SET
//! equality is what matters.
//!
//! GROUP BY queries are excluded (Phase05-issues §7 — accepted deviation).
//!
//! ## Prerequisites
//!
//! | Env var              | Default       | Description                                   |
//! |----------------------|---------------|-----------------------------------------------|
//! | `FORGEQL_DATA_DIR`   | *(required)*  | ForgeQL data dir with the source registered.  |
//! | `PARITY_SOURCE`      | `zephyr-andre`| Source name registered in that data dir.      |
//! | `PARITY_BRANCH`      | `main`        | Branch to open.                               |
//!
//! The test **skips** (prints a message and exits successfully) when
//! `FORGEQL_DATA_DIR` is not set or the source is not registered — it never
//! fails due to missing external infrastructure.
//!
//! TODO: Future iterations should connect to an already-running MCP server
//! (e.g. via socket / named pipe) instead of spawning one per test, so the
//! test no longer needs `FORGEQL_DATA_DIR` directly.
//!
//! ## Activation
//!
//! ```sh
//! FORGEQL_DATA_DIR=/path/to/data \
//!   cargo test --package forgeql --test parity_find -- --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

// ── session alias ─────────────────────────────────────────────────────────────

/// The alias used for `USE <source>.<branch> AS '<PARITY_ALIAS>'`.
/// Must differ from the branch name.  "parity" is always fine.
const PARITY_ALIAS: &str = "parity";

// ── MCP JSON-RPC client ───────────────────────────────────────────────────────

/// Minimal MCP JSON-RPC client over stdio for a `forgeql --mcp` subprocess.
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    /// Spawn `forgeql --mcp --log-queries --data-dir <data_dir>` and perform
    /// the MCP handshake.
    fn spawn(data_dir: &std::path::Path) -> std::io::Result<Self> {
        let binary = env!("CARGO_BIN_EXE_forgeql");
        let mut child = Command::new(binary)
            .arg("--mcp")
            .arg("--log-queries")
            .arg("--data-dir")
            .arg(data_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        client.handshake()?;
        Ok(client)
    }

    fn handshake(&mut self) -> std::io::Result<()> {
        let init = self.request(
            "initialize",
            &json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "parity_find", "version": "1.0"},
            }),
        )?;
        if init.get("error").is_some() {
            return Err(std::io::Error::other(format!("initialize failed: {init}")));
        }
        self.notify("notifications/initialized", &json!({}))?;
        Ok(())
    }

    fn send_line(&mut self, msg: &Value) -> std::io::Result<()> {
        let line = format!("{msg}\n");
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()
    }

    fn read_line(&mut self) -> std::io::Result<Value> {
        let mut buf = String::new();
        loop {
            buf.clear();
            let n = self.stdout.read_line(&mut buf)?;
            if n == 0 {
                return Err(std::io::Error::other("server closed stdout"));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed)
                .map_err(|e| std::io::Error::other(format!("json parse: {e} (line: {trimmed})")));
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> std::io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send_line(&req)?;
        // The server may emit notifications; loop until we get the matching response id.
        loop {
            let resp = self.read_line()?;
            if resp.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(resp);
            }
        }
    }

    fn notify(&mut self, method: &str, params: &Value) -> std::io::Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.send_line(&msg)
    }

    /// Call the `run_fql` tool and return the unwrapped result text (the
    /// tool's JSON payload, not the JSON-RPC envelope).
    fn run_fql(&mut self, session_id: Option<&str>, fql: &str) -> std::io::Result<String> {
        let mut args = json!({"fql": fql, "format": "JSON"});
        if let Some(sid) = session_id {
            args["session_id"] = json!(sid);
        }
        let resp = self.request(
            "tools/call",
            &json!({
                "name": "run_fql",
                "arguments": args,
            }),
        )?;
        if let Some(err) = resp.get("error") {
            return Err(std::io::Error::other(format!("tools/call error: {err}")));
        }
        // The structured JSON payload is the LAST content block. USE prepends a
        // human-readable "store this session_id" warning as content[0], so reading
        // content[0] would return prose, not JSON. Take the last text block, as the
        // zephyr/golden harnesses do.
        let text = resp
            .pointer("/result/content")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find_map(|c| c.get("text").and_then(Value::as_str))
            })
            .ok_or_else(|| std::io::Error::other(format!("unexpected response shape: {resp}")))?
            .to_owned();
        Ok(text)
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        // Closing stdin signals the server to shut down cleanly.
        // Then wait briefly; kill if it doesn't exit promptly.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── parity session ────────────────────────────────────────────────────────────

/// A live MCP session opened via `USE <source>.<branch> AS 'parity'`.
///
/// Returns `None` when the preconditions are unmet so the test can skip.
struct ParitySession {
    client: McpClient,
    session_id: String,
}

impl ParitySession {
    /// Try to open a parity session.
    fn connect() -> Option<Self> {
        let data_dir = if let Ok(v) = std::env::var("FORGEQL_DATA_DIR") {
            PathBuf::from(v)
        } else {
            eprintln!(
                "[parity_find] SKIP — FORGEQL_DATA_DIR not set.\n\
                 Set it to a ForgeQL data dir with the source registered:\n\
                 \n  FORGEQL_DATA_DIR=/path/to/data \
                 cargo test --package forgeql --test parity_find"
            );
            return None;
        };
        let source = std::env::var("PARITY_SOURCE").unwrap_or_else(|_| "zephyr-andre".into());
        let branch = std::env::var("PARITY_BRANCH").unwrap_or_else(|_| "main".into());

        let mut client = match McpClient::spawn(&data_dir) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("[parity_find] SKIP — failed to spawn MCP server: {err}");
                return None;
            }
        };

        let use_fql = format!("USE {source}.{branch} AS '{PARITY_ALIAS}'");
        // The server's session_id is the full `user:source:branch:alias` coordinate,
        // not the bare alias — read it from the USE response and use it for every
        // subsequent call. Hardcoding PARITY_ALIAS is rejected with
        // "invalid session id 'parity': expected 'user:source:branch:alias'".
        let session_id = match client.run_fql(None, &use_fql) {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("session_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| PARITY_ALIAS.to_owned()),
            Err(err) => {
                eprintln!(
                    "[parity_find] SKIP — USE {source}.{branch} AS '{PARITY_ALIAS}' failed: {err}\n\
                     Make sure '{source}' is registered in {dir}.",
                    dir = data_dir.display()
                );
                return None;
            }
        };

        eprintln!(
            "[parity_find] MCP session '{PARITY_ALIAS}' ready ({session_id}) — \
             source={source} branch={branch}"
        );
        Some(Self { client, session_id })
    }

    /// Execute a full FQL query string via `run_fql` and project results to
    /// `(name, kind, line)` sorted tuples.
    fn run(&mut self, query: &str) -> Vec<(String, String, usize)> {
        let text = self
            .client
            .run_fql(Some(&self.session_id), query)
            .unwrap_or_else(|e| panic!("run_fql({query}) failed: {e}"));
        extract_results(&text, query)
    }
}

// ── query helpers ─────────────────────────────────────────────────────────────

/// Convert `FIND symbols [clauses]` → `FIND symbols USING 'columnar' [clauses]`.
fn to_columnar(query: &str) -> String {
    query.strip_prefix("FIND symbols").map_or_else(
        || panic!("corpus query must start with 'FIND symbols': {query}"),
        |rest| format!("FIND symbols USING 'columnar'{rest}"),
    )
}

/// Parse the JSON payload returned by `run_fql` and project to sorted
/// `(name, kind, line)` tuples.
fn extract_results(text: &str, query: &str) -> Vec<(String, String, usize)> {
    let v: Value = serde_json::from_str(text)
        .unwrap_or_else(|e| panic!("invalid JSON for query [{query}]: {e}\npayload: {text}"));
    let rows = v
        .get("results")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing 'results' array in payload for [{query}]: {text}"));
    let mut out: Vec<(String, String, usize)> = rows
        .iter()
        .map(|r| {
            let name = r
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let kind = r
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let line = r
                .get("line")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .try_into()
                .unwrap_or(0usize);
            (name, kind, line)
        })
        .collect();
    out.sort_unstable();
    out
}

// ── corpus ────────────────────────────────────────────────────────────────────

/// Build the ≥200-query parity corpus as `(label, fql_query)` pairs.
///
/// Design rules (zephyr-andre corpus):
/// - Every query must return 0-15 results on zephyr-andre without any LIMIT,
///   so both backends return the full result set and ordering/set equality is
///   trivially comparable without a blanket LIMIT normalisation.
/// - Queries are constrained via path scoping (`IN 'path/**'`), exact name
///   matches, or tight multi-predicate combinations to stay within that range.
/// - LIMIT tests (g22) are dedicated: small LIMIT values on tight predicates
///   whose total result count is known to be small anyway, verifying that LIMIT
///   truncates identically on both backends.
/// - GROUP BY queries are excluded (Phase05-issues §7 — accepted deviation).
fn corpus() -> Vec<(String, String)> {
    let raw: Vec<[String; 2]> =
        serde_json::from_str(include_str!("corpus.json")).expect("parse corpus.json");
    raw.into_iter().map(Into::into).collect()
}

// ── failure formatting ────────────────────────────────────────────────────────

type QueryRow = (String, String, usize);
type ParityResult = (String, Vec<QueryRow>, Vec<QueryRow>);
type FailureRef<'a> = (&'a str, Vec<QueryRow>, Vec<QueryRow>);

fn format_failures(failures: &[FailureRef<'_>]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (label, legacy, columnar) in failures {
        let _ = write!(
            out,
            "\n  [{label}] legacy={} columnar={}\n",
            legacy.len(),
            columnar.len()
        );
        let max = legacy.len().max(columnar.len()).min(5);
        for i in 0..max {
            let l = legacy
                .get(i)
                .map_or_else(|| "<none>".to_owned(), |t| format!("{t:?}"));
            let c = columnar
                .get(i)
                .map_or_else(|| "<none>".to_owned(), |t| format!("{t:?}"));
            if legacy.get(i) != columnar.get(i) {
                let _ = writeln!(out, "    row {i}: legacy={l} columnar={c}");
            }
        }
    }
    out
}

// ── gate test ─────────────────────────────────────────────────────────────────

#[test]
fn parity_full_corpus() {
    let mut corpus = corpus();

    // Optional fast mode: when `PARITY_SHORT=1` is set, keep only the first
    // 2 queries of each `gNN_` group (≈50 queries instead of ≈250) so the
    // gate runs in ~minutes instead of ~16 minutes.  Use the full corpus
    // for nightly / pre-release runs by leaving the variable unset.
    let short = std::env::var("PARITY_SHORT").ok().is_some_and(|v| v == "1");
    if short {
        let mut per_group: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        corpus.retain(|(label, _)| {
            let group = label
                .split_once('_')
                .map_or_else(|| label.clone(), |(g, _)| g.to_owned());
            let n = per_group.entry(group).or_insert(0);
            *n += 1;
            *n <= 2
        });
        eprintln!(
            "PARITY_SHORT=1 → reduced corpus to {} queries",
            corpus.len()
        );
    } else {
        assert!(
            corpus.len() >= 200,
            "corpus must have ≥200 queries, has {}",
            corpus.len()
        );
    }

    // Connect via USE <source>.<branch> AS 'parity'.
    // Returns None and skips when FORGEQL_DATA_DIR is unset or source is not registered.
    let Some(mut parity) = ParitySession::connect() else {
        return;
    };

    // Run each query pair and collect results.
    //
    // Queries are designed to return 0-15 results naturally, so no LIMIT
    // normalisation is needed.  Queries with explicit LIMIT are tested as-is.
    let mut results: Vec<ParityResult> = Vec::new();

    for (label, query) in &corpus {
        let legacy = parity.run(query);
        let columnar_query = to_columnar(query);
        let columnar = parity.run(&columnar_query);
        results.push((label.clone(), legacy, columnar));
    }

    // Collect failures.
    let failures: Vec<FailureRef<'_>> = results
        .iter()
        .filter(|(_, l, c)| l != c)
        .map(|(label, l, c)| (label.as_str(), l.clone(), c.clone()))
        .collect();

    assert!(
        failures.is_empty(),
        "{} parity failures (out of {} corpus queries):{}",
        failures.len(),
        corpus.len(),
        format_failures(&failures)
    );
}
