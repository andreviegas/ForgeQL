//! Phase 0a — Golden-value integration test against the frozen `zephyr-main` branch.
//!
//! Opens a real MCP session against `zephyr-andre.zephyr-main` and asserts that
//! specific queries return the exact expected rows.  Values were recorded on
//! 2026-05-17 from a live MCP session; the branch **must never be rebased or
//! force-pushed** so these expectations remain permanently stable.
//!
//! Purpose: any refactor that breaks the overlay reader, the FST lookup, the bitmap
//! prefilter, or the row materialiser produces a clear diff here rather than a
//! silent regression.
//!
//! ## Prerequisites
//!
//! | Env var            | Default         | Description                                              |
//! |--------------------|-----------------|----------------------------------------------------------|
//! | `FORGEQL_DATA_DIR` | *(required)*    | ForgeQL data dir with `zephyr-andre` already registered. |
//! | `GOLDEN_SOURCE`    | `zephyr-andre`  | Source name in that data dir.                            |
//! | `GOLDEN_BRANCH`    | `zephyr-main`   | Branch to open (frozen — do not change).                 |
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

use serde_json::{Value, json};

// ── total symbol count from USE response ────────────────────────────────────

/// Recorded total when the session was first built against this commit.
const GOLDEN_SYMBOLS_INDEXED: usize = 2_720_018;

// ── alias for the test session ───────────────────────────────────────────────

const GOLDEN_ALIAS: &str = "golden";

// ── minimal MCP JSON-RPC client ─────────────────────────────────────────────

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
                "clientInfo": {"name": "zephyr_golden", "version": "1.0"},
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
        // When the response contains a session hint (e.g. from USE), the hint
        // is content[0] and the JSON body is the last item.  For plain queries
        // there is only one item.  Always take the last text-type content.
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

// ── result extraction helpers ────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    name: String,
    kind: String,
    line: u64,
    path: String,
}

fn extract_rows(payload: &Value, query: &str) -> Vec<Row> {
    let results = payload
        .get("results")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("missing 'results' array for [{query}]: {payload}"));
    results
        .iter()
        .map(|r| Row {
            name: r
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            kind: r
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            line: r.get("line").and_then(Value::as_u64).unwrap_or(0),
            path: r
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        })
        .collect()
}

fn total(payload: &Value) -> usize {
    payload
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .try_into()
        .unwrap_or(0)
}

// ── test ─────────────────────────────────────────────────────────────────────

#[test]
fn zephyr_golden_values() {
    // ── Skip guard ───────────────────────────────────────────────────────────
    let Ok(data_dir_str) = std::env::var("FORGEQL_DATA_DIR") else {
        eprintln!(
            "[zephyr_golden] SKIP — FORGEQL_DATA_DIR not set.\n\
             Set it to a ForgeQL data dir with 'zephyr-andre' registered:\n\
             \n  FORGEQL_DATA_DIR=/path/to/data \
             cargo test --package forgeql --test zephyr_golden"
        );
        return;
    };
    let data_dir = PathBuf::from(data_dir_str);
    let source = std::env::var("GOLDEN_SOURCE").unwrap_or_else(|_| "zephyr-andre".into());
    let branch = std::env::var("GOLDEN_BRANCH").unwrap_or_else(|_| "zephyr-main".into());

    // ── Spawn MCP server ─────────────────────────────────────────────────────
    let mut client = McpClient::spawn(&data_dir)
        .unwrap_or_else(|e| panic!("[zephyr_golden] failed to spawn MCP server: {e}"));

    // ── Open session ─────────────────────────────────────────────────────────
    let use_fql = format!("USE {source}.{branch} AS '{GOLDEN_ALIAS}'");
    let use_result = client.run_fql(None, &use_fql).unwrap_or_else(|e| {
        panic!(
            "[zephyr_golden] '{use_fql}' failed: {e}\n\
                 Ensure '{source}' is registered in {}.",
            data_dir.display()
        )
    });

    eprintln!("[zephyr_golden] session open — source={source} branch={branch}");

    // ── G1: total symbol count ────────────────────────────────────────────────
    // `symbols_indexed` is reported directly in the USE response payload.
    let symbols_indexed = usize::try_from(
        use_result
            .get("symbols_indexed")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    )
    .unwrap_or(0);
    assert_eq!(
        symbols_indexed, GOLDEN_SYMBOLS_INDEXED,
        "G1: expected {GOLDEN_SYMBOLS_INDEXED} symbols_indexed, got {symbols_indexed}"
    );
    eprintln!("[zephyr_golden] G1 PASS — symbols_indexed = {symbols_indexed}");

    // Extract the opaque session_id token returned by USE (format: user:source:branch:alias).
    let sid_owned = use_result
        .get("session_id")
        .and_then(Value::as_str)
        .unwrap_or(GOLDEN_ALIAS)
        .to_owned();
    let sid = sid_owned.as_str();

    // ── G2: first 5 functions in kernel/sched.c ordered by line ──────────────
    //
    // Frozen expected values (commit 2026-05-17):
    //   thread_runq    line  51
    //   curr_cpu_runq  line  71
    //   runq_add       line  80
    //   runq_remove    line  88
    //   runq_yield     line  96
    {
        const Q: &str = "FIND symbols WHERE fql_kind = 'function' IN 'kernel/sched.c' ORDER BY line ASC LIMIT 5";
        let payload = client
            .run_fql(Some(sid), Q)
            .unwrap_or_else(|e| panic!("G2: run_fql failed: {e}"));
        let rows = extract_rows(&payload, Q);
        assert_eq!(rows.len(), 5, "G2: expected 5 rows, got {}", rows.len());
        assert_eq!(rows[0].name, "thread_runq", "G2[0].name");
        assert_eq!(rows[0].line, 51, "G2[0].line");
        assert_eq!(rows[1].name, "curr_cpu_runq", "G2[1].name");
        assert_eq!(rows[1].line, 71, "G2[1].line");
        assert_eq!(rows[2].name, "runq_add", "G2[2].name");
        assert_eq!(rows[2].line, 80, "G2[2].line");
        assert_eq!(rows[3].name, "runq_remove", "G2[3].name");
        assert_eq!(rows[3].line, 88, "G2[3].line");
        assert_eq!(rows[4].name, "runq_yield", "G2[4].name");
        assert_eq!(rows[4].line, 96, "G2[4].line");
        eprintln!("[zephyr_golden] G2 PASS — kernel/sched.c first 5 functions by line");
    }

    // ── G3: k_mutex_lock — exactly one result at the known declaration ────────
    {
        const Q: &str = "FIND symbols WHERE name = 'k_mutex_lock'";
        let payload = client
            .run_fql(Some(sid), Q)
            .unwrap_or_else(|e| panic!("G3: run_fql failed: {e}"));
        let t = total(&payload);
        assert_eq!(t, 1, "G3: expected total=1, got {t}");
        let rows = extract_rows(&payload, Q);
        assert_eq!(rows.len(), 1, "G3: expected 1 row, got {}", rows.len());
        assert_eq!(rows[0].name, "k_mutex_lock", "G3.name");
        assert_eq!(rows[0].kind, "field", "G3.kind");
        assert_eq!(rows[0].line, 3525, "G3.line");
        assert_eq!(rows[0].path, "include/zephyr/kernel.h", "G3.path");
        eprintln!("[zephyr_golden] G3 PASS — k_mutex_lock at include/zephyr/kernel.h:3525");
    }

    // ── G4: first function alphabetically ─────────────────────────────────────
    {
        const Q: &str = "FIND symbols WHERE fql_kind = 'function' ORDER BY name ASC LIMIT 1";
        let payload = client
            .run_fql(Some(sid), Q)
            .unwrap_or_else(|e| panic!("G4: run_fql failed: {e}"));
        let rows = extract_rows(&payload, Q);
        assert_eq!(rows.len(), 1, "G4: expected 1 row, got {}", rows.len());
        assert_eq!(rows[0].name, "AGC_IRQHandler", "G4.name");
        assert_eq!(rows[0].line, 64, "G4.line");
        assert_eq!(
            rows[0].path, "modules/hal_silabs/simplicity_sdk/src/blob_stubs.c",
            "G4.path"
        );
        eprintln!("[zephyr_golden] G4 PASS — first function alphabetically = AGC_IRQHandler");
    }

    eprintln!("[zephyr_golden] ALL GOLDEN ASSERTIONS PASSED");
}
