//! Data-driven golden-value integration tests.
//!
//! Test cases are loaded from [`tests/golden.json`].  Each entry is either a
//! `USE` step (opens or switches a session) or a query step (runs FQL and
//! checks the response against declared expectations).
//!
//! ## Adding or changing a test
//!
//! Edit `crates/forgeql/tests/golden.json` — **no Rust changes required**.
//!
//! ### JSON entry types
//!
//! **USE step** — opens a session; all following query steps use this session
//! until the next USE step:
//! ```json
//! {
//!   "use": "source-name.branch-name",
//!   "alias": "my-alias",
//!   "expect_symbols_indexed": 12345
//! }
//! ```
//!
//! **Query step** — runs FQL and checks the response:
//! ```json
//! {
//!   "name": "human_readable_test_name",
//!   "fql": "FIND symbols WHERE ...",
//!   "expect_total": 1,
//!   "expect_row_count": 5,
//!   "expect_rows": [
//!     {"name": "foo", "kind": "function", "line": 42, "path": "src/foo.c"}
//!   ]
//! }
//! ```
//!
//! All fields except `name` and `fql` are optional.  `expect_rows[i]` checks
//! only the fields listed — missing fields are ignored.
//!
//! ## Prerequisites
//!
//! | Env var            | Description                                            |
//! |--------------------|--------------------------------------------------------|
//! | `FORGEQL_DATA_DIR` | ForgeQL data dir with all required sources registered. |
//!
//! The test **skips** when `FORGEQL_DATA_DIR` is unset; it never fails due to
//! missing infrastructure.
//!
//! ## Activation
//!
//! ```sh
//! FORGEQL_DATA_DIR=/path/to/data \
//!   cargo test --package forgeql --test zephyr_golden -- --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::Deserialize;
use serde_json::{Value, json};

// ── golden entry types (deserialised from golden.json) ───────────────────────

/// A single step in the golden test suite.
///
/// - [`GoldenEntry::Use`] — opens (or switches) a session.
/// - [`GoldenEntry::Query`] — runs FQL and checks the response.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum GoldenEntry {
    Use(UseEntry),
    Query(QueryEntry),
}

/// `USE source.branch AS 'alias'` step.
#[derive(Debug, Deserialize)]
struct UseEntry {
    /// `"source.branch"` fed directly into `USE … AS`.
    #[serde(rename = "use")]
    use_str: String,
    /// Session alias (the `AS 'alias'` part).
    alias: String,
    /// If set, asserts `symbols_indexed` in the USE response equals this value.
    #[serde(default)]
    expect_symbols_indexed: Option<usize>,
}

/// FQL query step with expected results.
#[derive(Debug, Deserialize)]
struct QueryEntry {
    /// Human-readable test name printed on pass/fail.
    name: String,
    /// FQL statement to execute.
    fql: String,
    /// If set, asserts the `"total"` field in the response equals this value.
    #[serde(default)]
    expect_total: Option<usize>,
    /// If set, asserts the number of rows in `"results"` (or `content.files`
    /// for `FIND files`) equals this value.
    #[serde(default)]
    expect_row_count: Option<usize>,
    /// Per-row field matchers for `FIND` responses (`results` / `content.files`).
    /// Row `i` must contain every field listed; unlisted fields are ignored.
    #[serde(default)]
    expect_rows: Vec<Value>,
    /// If set, asserts the number of lines in `content.lines` (for `SHOW LINES`).
    #[serde(default)]
    expect_line_count: Option<usize>,
    /// Per-line field matchers for `SHOW LINES` responses (`content.lines`).
    /// Line `i` must contain every field listed; unlisted fields are ignored.
    #[serde(default)]
    expect_lines: Vec<Value>,
}

// ── minimal MCP JSON-RPC client ──────────────────────────────────────────────

struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpClient {
    fn spawn(data_dir: &std::path::Path) -> std::io::Result<Self> {
        let binary = env!("CARGO_BIN_EXE_forgeql");
        let mut child = Command::new(binary)
            .arg("--mcp")
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--log-queries")
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
                "clientInfo": {"name": "golden_test", "version": "1.0"},
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
        loop {
            let resp = self.read_line()?;
            if resp.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(resp);
            }
        }
    }

    fn notify(&mut self, method: &str, params: &Value) -> std::io::Result<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.send_line(&msg)
    }

    /// Call `run_fql` and return the decoded JSON payload.
    fn run_fql(&mut self, session_id: Option<&str>, fql: &str) -> std::io::Result<Value> {
        let mut args = json!({"fql": fql, "format": "JSON"});
        if let Some(sid) = session_id {
            args["session_id"] = json!(sid);
        }
        let resp = self.request(
            "tools/call",
            &json!({ "name": "run_fql", "arguments": args }),
        )?;
        if let Some(err) = resp.get("error") {
            return Err(std::io::Error::other(format!("tools/call error: {err}")));
        }
        let content = resp
            .pointer("/result/content")
            .and_then(Value::as_array)
            .ok_or_else(|| std::io::Error::other(format!("unexpected response shape: {resp}")))?;
        // USE responses carry a session hint at content[0]; the JSON body is
        // always the last content item.  For plain queries there is only one.
        let text = content
            .iter()
            .rev()
            .find_map(|c| c.get("text").and_then(Value::as_str))
            .ok_or_else(|| std::io::Error::other(format!("no text content in: {resp}")))?;
        serde_json::from_str(text)
            .map_err(|e| std::io::Error::other(format!("result JSON parse: {e}\npayload: {text}")))
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── test ─────────────────────────────────────────────────────────────────────

#[test]
fn golden_values() {
    // ── Skip guard ────────────────────────────────────────────────────────────
    let Ok(data_dir_str) = std::env::var("FORGEQL_DATA_DIR") else {
        eprintln!(
            "[golden] SKIP — FORGEQL_DATA_DIR not set.\n\
             Set it to a ForgeQL data dir with all required sources registered:\n\
             \n  FORGEQL_DATA_DIR=/path/to/data \
             cargo test --package forgeql --test zephyr_golden -- --nocapture"
        );
        return;
    };
    let data_dir = PathBuf::from(data_dir_str);

    // ── Load fixture ──────────────────────────────────────────────────────────
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden.json");
    let fixture_str = std::fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("[golden] cannot read {}: {e}", fixture_path.display()));
    let entries: Vec<GoldenEntry> = serde_json::from_str(&fixture_str)
        .unwrap_or_else(|e| panic!("[golden] cannot parse {}: {e}", fixture_path.display()));

    // ── Spawn MCP server ──────────────────────────────────────────────────────
    let mut client = McpClient::spawn(&data_dir)
        .unwrap_or_else(|e| panic!("[golden] failed to spawn MCP server: {e}"));

    let mut session_id: Option<String> = None;
    let mut failures: Vec<String> = Vec::new();
    let mut pass = 0usize;

    // ── Run entries ───────────────────────────────────────────────────────────
    for entry in &entries {
        match entry {
            // ── USE step ──────────────────────────────────────────────────────
            GoldenEntry::Use(u) => {
                let fql = format!("USE {} AS '{}'", u.use_str, u.alias);
                let result = client.run_fql(None, &fql).unwrap_or_else(|e| {
                    panic!(
                        "[golden] '{fql}' failed: {e}\n\
                         Ensure '{}' is registered in {}.",
                        u.use_str,
                        data_dir.display()
                    )
                });
                session_id = Some(
                    result
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or(&u.alias)
                        .to_owned(),
                );
                eprintln!(
                    "[golden] USE {} — sid={}",
                    u.use_str,
                    session_id.as_deref().unwrap_or("?")
                );
                if let Some(expected) = u.expect_symbols_indexed {
                    let got = usize::try_from(
                        result
                            .get("symbols_indexed")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    )
                    .unwrap_or(0);
                    if got == expected {
                        eprintln!("[golden] USE {} symbols_indexed={got} ✓", u.use_str);
                        pass += 1;
                    } else {
                        failures.push(format!(
                            "USE {}: symbols_indexed expected {expected}, got {got}",
                            u.use_str
                        ));
                    }
                }
            }

            // ── Query step ────────────────────────────────────────────────────
            GoldenEntry::Query(q) => {
                let result = client
                    .run_fql(session_id.as_deref(), &q.fql)
                    .unwrap_or_else(|e| panic!("[golden] '{}' run_fql failed: {e}", q.name));

                let mut entry_failures: Vec<String> = Vec::new();

                if let Some(expected_total) = q.expect_total {
                    let got =
                        usize::try_from(result.get("total").and_then(Value::as_u64).unwrap_or(0))
                            .unwrap_or(0);
                    if got != expected_total {
                        entry_failures.push(format!("total: expected {expected_total}, got {got}"));
                    }
                }

                // FIND symbols / globals → "results"
                // FIND files            → "content/files"
                let rows: &[Value] = result
                    .get("results")
                    .or_else(|| result.pointer("/content/files"))
                    .and_then(Value::as_array)
                    .map_or(&[], Vec::as_slice);

                if let Some(expected_count) = q.expect_row_count
                    && rows.len() != expected_count
                {
                    entry_failures.push(format!(
                        "row_count: expected {expected_count}, got {}",
                        rows.len()
                    ));
                }

                for (i, expected_row) in q.expect_rows.iter().enumerate() {
                    let Some(obj) = expected_row.as_object() else {
                        continue;
                    };
                    let Some(actual_row) = rows.get(i) else {
                        entry_failures.push(format!(
                            "row[{i}] missing (only {} rows returned)",
                            rows.len()
                        ));
                        continue;
                    };
                    for (field, expected_val) in obj {
                        let actual_val = actual_row.get(field).unwrap_or(&Value::Null);
                        if actual_val != expected_val {
                            entry_failures.push(format!(
                                "row[{i}].{field}: expected {expected_val}, got {actual_val}"
                            ));
                        }
                    }
                }

                // ── SHOW LINES assertions ─────────────────────────────────
                let lines: &[Value] = result
                    .pointer("/content/lines")
                    .and_then(Value::as_array)
                    .map_or(&[], Vec::as_slice);

                if let Some(expected_count) = q.expect_line_count
                    && lines.len() != expected_count
                {
                    entry_failures.push(format!(
                        "line_count: expected {expected_count}, got {}",
                        lines.len()
                    ));
                }

                for (i, expected_line) in q.expect_lines.iter().enumerate() {
                    let Some(obj) = expected_line.as_object() else {
                        continue;
                    };
                    let Some(actual_line) = lines.get(i) else {
                        entry_failures.push(format!(
                            "line[{i}] missing (only {} lines returned)",
                            lines.len()
                        ));
                        continue;
                    };
                    for (field, expected_val) in obj {
                        let actual_val = actual_line.get(field).unwrap_or(&Value::Null);
                        if actual_val != expected_val {
                            entry_failures.push(format!(
                                "line[{i}].{field}: expected {expected_val}, got {actual_val}"
                            ));
                        }
                    }
                }

                if entry_failures.is_empty() {
                    eprintln!("[golden] PASS {}", q.name);
                    pass += 1;
                } else {
                    for msg in &entry_failures {
                        eprintln!("[golden] FAIL {} — {msg}", q.name);
                        failures.push(format!("{}: {msg}", q.name));
                    }
                }
            }
        }
    }

    eprintln!("[golden] {pass} passed, {} failed", failures.len());
    assert!(
        failures.is_empty(),
        "{} golden assertion(s) failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
