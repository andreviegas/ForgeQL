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
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "parity_find", "version": "1.0"},
            }),
        )?;
        if init.get("error").is_some() {
            return Err(std::io::Error::other(format!("initialize failed: {init}")));
        }
        self.notify("notifications/initialized", json!({}))?;
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

    fn request(&mut self, method: &str, params: Value) -> std::io::Result<Value> {
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

    fn notify(&mut self, method: &str, params: Value) -> std::io::Result<()> {
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
            json!({
                "name": "run_fql",
                "arguments": args,
            }),
        )?;
        if let Some(err) = resp.get("error") {
            return Err(std::io::Error::other(format!("tools/call error: {err}")));
        }
        let text = resp
            .pointer("/result/content/0/text")
            .and_then(Value::as_str)
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
        match client.run_fql(None, &use_fql) {
            Ok(_) => {}
            Err(err) => {
                eprintln!(
                    "[parity_find] SKIP — USE {source}.{branch} AS '{PARITY_ALIAS}' failed: {err}\n\
                     Make sure '{source}' is registered in {dir}.",
                    dir = data_dir.display()
                );
                return None;
            }
        }

        eprintln!(
            "[parity_find] MCP session '{PARITY_ALIAS}' ready — source={source} branch={branch}"
        );
        Some(Self {
            client,
            session_id: PARITY_ALIAS.to_owned(),
        })
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
/// Every query starts with `FIND symbols`.  GROUP BY queries are excluded
/// (Phase05-issues §7 — accepted deviation).
fn corpus() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();

    macro_rules! q {
        ($label:expr, $fql:expr) => {
            v.push(($label.to_owned(), $fql.to_owned()));
        };
    }

    // ── Group 1: Unconstrained (1) ─────────────────────────────────────────
    q!("g01_all", "FIND symbols");

    // ── Group 2: Exact fql_kind = X (8) ────────────────────────────────────
    for k in [
        "function",
        "struct",
        "enum",
        "variable",
        "constant",
        "field",
        "enum_variant",
        "nonexistent_kind_xyz",
    ] {
        q!(
            format!("g02_kind_eq_{k}"),
            format!("FIND symbols WHERE fql_kind = '{k}'")
        );
    }

    // ── Group 3: fql_kind != X (5) ─────────────────────────────────────────
    for k in ["function", "struct", "enum", "variable", "constant"] {
        q!(
            format!("g03_kind_ne_{k}"),
            format!("FIND symbols WHERE fql_kind != '{k}'")
        );
    }

    // ── Group 4: Exact name match (24) ──────────────────────────────────────
    for n in [
        "foo",
        "bar",
        "factorial",
        "process",
        "helper",
        "transform",
        "checker",
        "shadowed",
        "escaping",
        "switcher",
        "distant",
        "caller",
        "noop",
        "no_default",
        "deeply_nested",
        "Motor",
        "State",
        "speed",
        "count",
        "hex_value",
        "bin_value",
        "pi",
        "MAGIC",
        "Idle",
    ] {
        q!(
            format!("g04_name_eq_{n}"),
            format!("FIND symbols WHERE name = '{n}'")
        );
    }

    // ── Group 5: Name != X (6) ──────────────────────────────────────────────
    for n in ["foo", "bar", "Motor", "State", "MAGIC", "count"] {
        q!(
            format!("g05_name_ne_{n}"),
            format!("FIND symbols WHERE name != '{n}'")
        );
    }

    // ── Group 6: LIKE prefix (14) ────────────────────────────────────────────
    for p in [
        "f%", "b%", "p%", "h%", "c%", "s%", "e%", "d%", "n%", "t%", "M%", "S%", "I%", "R%",
    ] {
        q!(
            format!("g06_name_like_{p}"),
            format!("FIND symbols WHERE name LIKE '{p}'")
        );
    }

    // ── Group 7: LIKE suffix (7) ─────────────────────────────────────────────
    for p in ["%er", "%or", "%ed", "%al", "%t", "%e", "%d"] {
        q!(
            format!("g07_name_like_{p}"),
            format!("FIND symbols WHERE name LIKE '{p}'")
        );
    }

    // ── Group 8: LIKE contains (7) ───────────────────────────────────────────
    for p in ["%oo%", "%ar%", "%at%", "%or%", "%ee%", "%al%", "%er%"] {
        q!(
            format!("g08_name_like_{p}"),
            format!("FIND symbols WHERE name LIKE '{p}'")
        );
    }

    // ── Group 9: NOT LIKE (8) ────────────────────────────────────────────────
    for p in [
        "f%",
        "b%",
        "M%",
        "S%",
        "%er",
        "%ed",
        "%oo%",
        "nonexistent_prefix_xyz%",
    ] {
        q!(
            format!("g09_name_not_like_{p}"),
            format!("FIND symbols WHERE name NOT LIKE '{p}'")
        );
    }

    // ── Group 10: Enrichment field = value (6) ───────────────────────────────
    for (f, val) in [
        ("has_doc", "true"),
        ("has_doc", "false"),
        ("is_recursive", "true"),
        ("is_recursive", "false"),
        ("has_fallthrough", "true"),
        ("has_fallthrough", "false"),
    ] {
        q!(
            format!("g10_field_{f}_{val}"),
            format!("FIND symbols WHERE {f} = '{val}'")
        );
    }

    // ── Group 11: Enrichment field != value (4) ──────────────────────────────
    for (f, val) in [
        ("has_doc", "true"),
        ("has_doc", "false"),
        ("is_recursive", "true"),
        ("is_recursive", "false"),
    ] {
        q!(
            format!("g11_field_ne_{f}_{val}"),
            format!("FIND symbols WHERE {f} != '{val}'")
        );
    }

    // ── Group 12: Line numeric predicates (8) ────────────────────────────────
    for (label, pred) in [
        ("gt_0", "line > 0"),
        ("gt_10", "line > 10"),
        ("gt_30", "line > 30"),
        ("ge_1", "line >= 1"),
        ("ge_20", "line >= 20"),
        ("lt_50", "line < 50"),
        ("lt_100", "line < 100"),
        ("le_20", "line <= 20"),
    ] {
        q!(
            format!("g12_line_{label}"),
            format!("FIND symbols WHERE {pred}")
        );
    }

    // ── Group 13: fql_kind + exact name (10) ─────────────────────────────────
    for (k, n) in [
        ("function", "foo"),
        ("function", "bar"),
        ("function", "factorial"),
        ("function", "noop"),
        ("function", "caller"),
        ("struct", "Motor"),
        ("enum", "State"),
        ("field", "speed"),
        ("variable", "count"),
        ("constant", "MAGIC"),
    ] {
        q!(
            format!("g13_kind_{k}_name_{n}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE name = '{n}'")
        );
    }

    // ── Group 14: fql_kind + LIKE (14) ───────────────────────────────────────
    for (k, p) in [
        ("function", "f%"),
        ("function", "b%"),
        ("function", "c%"),
        ("function", "s%"),
        ("function", "h%"),
        ("function", "d%"),
        ("function", "n%"),
        ("function", "t%"),
        ("function", "e%"),
        ("function", "%er"),
        ("function", "%ed"),
        ("struct", "M%"),
        ("enum", "S%"),
        ("variable", "c%"),
    ] {
        q!(
            format!("g14_kind_{k}_like_{p}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE name LIKE '{p}'")
        );
    }

    // ── Group 15: fql_kind + enrichment (8) ──────────────────────────────────
    for (k, f, val) in [
        ("function", "has_doc", "true"),
        ("function", "has_doc", "false"),
        ("function", "is_recursive", "true"),
        ("function", "is_recursive", "false"),
        ("struct", "has_doc", "true"),
        ("struct", "has_doc", "false"),
        ("enum", "has_doc", "true"),
        ("enum", "has_doc", "false"),
    ] {
        q!(
            format!("g15_kind_{k}_{f}_{val}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE {f} = '{val}'")
        );
    }

    // ── Group 16: fql_kind + line range (6) ──────────────────────────────────
    for (k, label, pred) in [
        ("function", "gt_5", "line > 5"),
        ("function", "gt_20", "line > 20"),
        ("function", "lt_50", "line < 50"),
        ("variable", "gt_0", "line > 0"),
        ("struct", "gt_0", "line > 0"),
        ("enum", "gt_0", "line > 0"),
    ] {
        q!(
            format!("g16_kind_{k}_line_{label}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE {pred}")
        );
    }

    // ── Group 17: LIKE + enrichment (8) ──────────────────────────────────────
    for (p, f, val) in [
        ("f%", "has_doc", "true"),
        ("f%", "has_doc", "false"),
        ("b%", "has_doc", "true"),
        ("c%", "has_doc", "true"),
        ("s%", "has_doc", "true"),
        ("h%", "is_recursive", "false"),
        ("n%", "has_doc", "false"),
        ("t%", "has_doc", "true"),
    ] {
        q!(
            format!("g17_like_{p}_{f}_{val}"),
            format!("FIND symbols WHERE name LIKE '{p}' WHERE {f} = '{val}'")
        );
    }

    // ── Group 18: ORDER BY field ASC (5) ─────────────────────────────────────
    for f in ["name", "line", "usages", "fql_kind", "language"] {
        q!(
            format!("g18_order_asc_{f}"),
            format!("FIND symbols ORDER BY {f} ASC")
        );
    }

    // ── Group 19: ORDER BY field DESC (5) ────────────────────────────────────
    for f in ["name", "line", "usages", "fql_kind", "language"] {
        q!(
            format!("g19_order_desc_{f}"),
            format!("FIND symbols ORDER BY {f} DESC")
        );
    }

    // ── Group 20: fql_kind + ORDER BY (10) ───────────────────────────────────
    for (k, f, dir) in [
        ("function", "name", "ASC"),
        ("function", "name", "DESC"),
        ("function", "line", "ASC"),
        ("function", "line", "DESC"),
        ("function", "usages", "ASC"),
        ("struct", "name", "ASC"),
        ("enum", "name", "ASC"),
        ("variable", "name", "ASC"),
        ("variable", "line", "ASC"),
        ("constant", "name", "ASC"),
    ] {
        q!(
            format!("g20_kind_{k}_order_{f}_{}", dir.to_lowercase()),
            format!("FIND symbols WHERE fql_kind = '{k}' ORDER BY {f} {dir}")
        );
    }

    // ── Group 21: LIKE + ORDER BY (10) ───────────────────────────────────────
    for (p, f, dir) in [
        ("f%", "name", "ASC"),
        ("f%", "line", "ASC"),
        ("b%", "name", "ASC"),
        ("c%", "name", "ASC"),
        ("s%", "name", "ASC"),
        ("h%", "name", "ASC"),
        ("d%", "line", "ASC"),
        ("%er", "name", "ASC"),
        ("%ed", "name", "ASC"),
        ("n%", "line", "ASC"),
    ] {
        q!(
            format!("g21_like_{p}_order_{f}_{}", dir.to_lowercase()),
            format!("FIND symbols WHERE name LIKE '{p}' ORDER BY {f} {dir}")
        );
    }

    // ── Group 22: LIMIT=1000 (large, effectively no limit) (5) ──────────────
    for (label, base) in [
        ("all", "FIND symbols"),
        ("function", "FIND symbols WHERE fql_kind = 'function'"),
        ("like_f", "FIND symbols WHERE name LIKE 'f%'"),
        ("has_doc_true", "FIND symbols WHERE has_doc = 'true'"),
        ("name_eq_foo", "FIND symbols WHERE name = 'foo'"),
    ] {
        q!(
            format!("g22_limit1000_{label}"),
            format!("{base} LIMIT 1000")
        );
    }

    // ── Group 23: LIMIT=1000 + ORDER BY (8) ──────────────────────────────────
    for (label, base, ord) in [
        ("all_name_asc", "FIND symbols", "ORDER BY name ASC"),
        ("all_name_desc", "FIND symbols", "ORDER BY name DESC"),
        ("all_line_asc", "FIND symbols", "ORDER BY line ASC"),
        (
            "fn_name_asc",
            "FIND symbols WHERE fql_kind = 'function'",
            "ORDER BY name ASC",
        ),
        (
            "fn_line_asc",
            "FIND symbols WHERE fql_kind = 'function'",
            "ORDER BY line ASC",
        ),
        (
            "like_f_name_asc",
            "FIND symbols WHERE name LIKE 'f%'",
            "ORDER BY name ASC",
        ),
        (
            "like_c_line_asc",
            "FIND symbols WHERE name LIKE 'c%'",
            "ORDER BY line ASC",
        ),
        (
            "has_doc_name_asc",
            "FIND symbols WHERE has_doc = 'true'",
            "ORDER BY name ASC",
        ),
    ] {
        q!(
            format!("g23_lim1000_{label}"),
            format!("{base} {ord} LIMIT 1000")
        );
    }

    // ── Group 24: OFFSET + large LIMIT (8) ───────────────────────────────────
    for off in [0usize, 5, 10, 20] {
        for (base_label, base) in [
            ("all", "FIND symbols"),
            ("fn", "FIND symbols WHERE fql_kind = 'function'"),
        ] {
            q!(
                format!("g24_off{off}_{base_label}"),
                format!("{base} ORDER BY name ASC LIMIT 1000 OFFSET {off}")
            );
        }
    }

    // ── Group 25: Triple combos — kind + LIKE + enrichment (8) ───────────────
    for (k, p, f, val) in [
        ("function", "f%", "has_doc", "true"),
        ("function", "f%", "has_doc", "false"),
        ("function", "b%", "has_doc", "true"),
        ("function", "c%", "is_recursive", "false"),
        ("function", "s%", "has_doc", "true"),
        ("function", "h%", "has_doc", "true"),
        ("function", "n%", "has_doc", "false"),
        ("function", "t%", "is_recursive", "true"),
    ] {
        q!(
            format!("g25_{k}_{p}_{f}_{val}"),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE name LIKE '{p}' WHERE {f} = '{val}'"
            )
        );
    }

    // ── Group 26: kind + NOT LIKE (6) ─────────────────────────────────────────
    for (k, p) in [
        ("function", "f%"),
        ("function", "b%"),
        ("function", "%er"),
        ("function", "%ed"),
        ("variable", "c%"),
        ("struct", "nonexistent%"),
    ] {
        q!(
            format!("g26_kind_{k}_not_like_{p}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE name NOT LIKE '{p}'")
        );
    }

    // ── Group 27: name + enrichment (6) ──────────────────────────────────────
    for (n, f, val) in [
        ("foo", "has_doc", "true"),
        ("foo", "has_doc", "false"),
        ("bar", "has_doc", "true"),
        ("factorial", "is_recursive", "true"),
        ("noop", "has_doc", "false"),
        ("Motor", "has_doc", "true"),
    ] {
        q!(
            format!("g27_name_{n}_{f}_{val}"),
            format!("FIND symbols WHERE name = '{n}' WHERE {f} = '{val}'")
        );
    }

    // ── Group 28: line range + kind (6) ──────────────────────────────────────
    for (k, pred, label) in [
        ("function", "line > 0", "gt0"),
        ("function", "line > 10", "gt10"),
        ("function", "line < 80", "lt80"),
        ("function", "line >= 5", "ge5"),
        ("variable", "line > 0", "gt0"),
        ("enum", "line > 0", "gt0"),
    ] {
        q!(
            format!("g28_{k}_line_{label}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE {pred}")
        );
    }

    // ── Group 29: more name combos + ORDER BY (8) ─────────────────────────────
    for (n, f, dir) in [
        ("foo", "line", "ASC"),
        ("bar", "line", "ASC"),
        ("factorial", "name", "ASC"),
        ("caller", "line", "ASC"),
        ("shadowed", "line", "ASC"),
        ("noop", "name", "ASC"),
        ("Motor", "name", "ASC"),
        ("MAGIC", "line", "ASC"),
    ] {
        q!(
            format!("g29_name_{n}_order_{f}_{}", dir.to_lowercase()),
            format!("FIND symbols WHERE name = '{n}' ORDER BY {f} {dir}")
        );
    }

    // ── Group 30: Multiple enrichment predicates (6) ──────────────────────────
    for (f1, v1, f2, v2) in [
        ("has_doc", "true", "is_recursive", "true"),
        ("has_doc", "true", "is_recursive", "false"),
        ("has_doc", "false", "is_recursive", "false"),
        ("has_doc", "true", "has_fallthrough", "false"),
        ("has_doc", "false", "has_fallthrough", "false"),
        ("is_recursive", "true", "has_fallthrough", "false"),
    ] {
        q!(
            format!("g30_{f1}_{v1}_{f2}_{v2}"),
            format!("FIND symbols WHERE {f1} = '{v1}' WHERE {f2} = '{v2}'")
        );
    }

    // ── Group 31: kind + line range + ORDER BY (6) ────────────────────────────
    for (k, line_pred, line_label, ord_f, ord_dir) in [
        ("function", "line > 0", "gt0", "line", "ASC"),
        ("function", "line > 10", "gt10", "line", "ASC"),
        ("function", "line > 0", "gt0", "name", "ASC"),
        ("variable", "line > 0", "gt0", "line", "ASC"),
        ("struct", "line > 0", "gt0", "name", "ASC"),
        ("enum", "line > 0", "gt0", "name", "ASC"),
    ] {
        q!(
            format!(
                "g31_{k}_line_{line_label}_order_{ord_f}_{}",
                ord_dir.to_lowercase()
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE {line_pred} ORDER BY {ord_f} {ord_dir}"
            )
        );
    }

    // ── Group 32: LIKE prefix + kind + ORDER BY (8) ───────────────────────────
    for (k, p, ord_f) in [
        ("function", "f%", "name"),
        ("function", "b%", "name"),
        ("function", "c%", "line"),
        ("function", "s%", "name"),
        ("function", "h%", "line"),
        ("function", "n%", "name"),
        ("variable", "c%", "line"),
        ("struct", "M%", "name"),
    ] {
        q!(
            format!("g32_kind_{k}_like_{p}_order_{ord_f}"),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE name LIKE '{p}' ORDER BY {ord_f} ASC"
            )
        );
    }

    // ── Group 33: Exact name + kind (negative — no match) (5) ────────────────
    for (k, n) in [
        ("struct", "foo"),     // foo is a function, not struct
        ("function", "Motor"), // Motor is a struct, not function
        ("variable", "MAGIC"), // MAGIC is constant, not variable
        ("enum", "speed"),     // speed is a field, not enum
        ("constant", "count"), // count is variable, not constant
    ] {
        q!(
            format!("g33_empty_kind_{k}_name_{n}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE name = '{n}'")
        );
    }

    // ── Group 34: Clearly empty queries (5) ───────────────────────────────────
    q!(
        "g34_empty_name_xyz",
        "FIND symbols WHERE name = 'nonexistent_xyz_abc_123'"
    );
    q!(
        "g34_empty_kind_xyz",
        "FIND symbols WHERE fql_kind = 'nonexistent_kind_xyz'"
    );
    q!(
        "g34_empty_like_zzz",
        "FIND symbols WHERE name LIKE 'zzz_nomatch_%'"
    );
    q!("g34_empty_line_gt9999", "FIND symbols WHERE line > 9999");
    q!("g34_empty_line_lt0", "FIND symbols WHERE line < 0");

    // ── Group 35: kind != + LIKE (5) ──────────────────────────────────────────
    for (k, p) in [
        ("function", "M%"),
        ("struct", "f%"),
        ("enum", "f%"),
        ("variable", "M%"),
        ("constant", "f%"),
    ] {
        q!(
            format!("g35_kind_ne_{k}_like_{p}"),
            format!("FIND symbols WHERE fql_kind != '{k}' WHERE name LIKE '{p}'")
        );
    }

    // ── Group 36: kind + enrichment + ORDER BY (6) ────────────────────────────
    for (k, f, val, ord) in [
        ("function", "has_doc", "true", "name"),
        ("function", "has_doc", "false", "name"),
        ("function", "is_recursive", "true", "line"),
        ("function", "is_recursive", "false", "name"),
        ("struct", "has_doc", "true", "name"),
        ("enum", "has_doc", "true", "name"),
    ] {
        q!(
            format!("g36_{k}_{f}_{val}_order_{ord}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE {f} = '{val}' ORDER BY {ord} ASC")
        );
    }

    // ── Group 37: name LIKE suffix + kind (5) ────────────────────────────────
    for (p, k) in [
        ("%er", "function"),
        ("%ed", "function"),
        ("%al", "function"),
        ("%or", "struct"),
        ("%e", "function"),
    ] {
        q!(
            format!("g37_like_{p}_kind_{k}"),
            format!("FIND symbols WHERE name LIKE '{p}' WHERE fql_kind = '{k}'")
        );
    }

    // ── Group 38: More exact names (4) ────────────────────────────────────────
    for n in ["Running", "Stopped", "deeply_nested", "no_default"] {
        q!(
            format!("g38_name_eq_{n}"),
            format!("FIND symbols WHERE name = '{n}'")
        );
    }

    // ── Group 39: usages numeric predicates (4) ────────────────────────────────
    for (pred, label) in [
        ("usages >= 0", "ge0"),
        ("usages > 0", "gt0"),
        ("usages <= 100", "le100"),
        ("usages >= 1", "ge1"),
    ] {
        q!(
            format!("g39_usages_{label}"),
            format!("FIND symbols WHERE {pred}")
        );
    }

    // ── Group 40: kind + usages predicates (4) ────────────────────────────────
    for (k, pred, label) in [
        ("function", "usages >= 0", "ge0"),
        ("function", "usages > 0", "gt0"),
        ("struct", "usages >= 0", "ge0"),
        ("variable", "usages >= 0", "ge0"),
    ] {
        q!(
            format!("g40_{k}_usages_{label}"),
            format!("FIND symbols WHERE fql_kind = '{k}' WHERE {pred}")
        );
    }

    v
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
    let corpus = corpus();
    assert!(
        corpus.len() >= 200,
        "corpus must have ≥200 queries, has {}",
        corpus.len()
    );

    // Connect via USE <source>.<branch> AS 'parity'.
    // Returns None and skips when FORGEQL_DATA_DIR is unset or source is not registered.
    let Some(mut parity) = ParitySession::connect() else {
        return;
    };

    // Run each query pair and collect results.
    //
    // Append `LIMIT 1000` to queries without an explicit LIMIT so both backends
    // return ALL matching symbols rather than the first 20 in their respective
    // iteration orders — iteration order differs between legacy and columnar,
    // so comparing with default LIMIT 20 would produce spurious divergences.
    let mut results: Vec<ParityResult> = Vec::new();

    for (label, query) in &corpus {
        let run_query = if query.contains(" LIMIT ") {
            query.clone()
        } else {
            format!("{query} LIMIT 1000")
        };
        let legacy = parity.run(&run_query);
        let columnar_query = to_columnar(&run_query);
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
