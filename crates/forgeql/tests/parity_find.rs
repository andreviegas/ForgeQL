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
    let mut v: Vec<(String, String)> = Vec::new();

    macro_rules! q {
        ($label:expr, $fql:expr) => {
            v.push(($label.to_owned(), $fql.to_owned()));
        };
    }

    // ── Group 1: Exact name match — zephyr symbols (20) ───────────────────────
    // These return a small fixed count (1-10) from the real index.
    for n in [
        "stopped", "running", "idle", "init", "reset", "enable", "disable", "start", "stop",
        "send", "recv", "open", "close", "read", "write", "flush", "abort", "cancel", "attach",
        "detach",
    ] {
        q!(
            format!("g01_name_eq_{n}"),
            format!("FIND symbols WHERE name = '{n}'")
        );
    }

    // ── Group 2: Exact fql_kind + scoped path (10) ────────────────────────────
    // Path-scoped to small subtrees so results stay ≤15.
    for (k, path) in [
        ("function", "drivers/serial/**"),
        ("struct", "drivers/serial/**"),
        ("enum", "drivers/serial/**"),
        ("variable", "drivers/serial/**"),
        ("function", "drivers/gpio/**"),
        ("struct", "drivers/gpio/**"),
        ("function", "drivers/i2c/**"),
        ("struct", "drivers/i2c/**"),
        ("function", "drivers/spi/**"),
        ("enum", "drivers/bluetooth/**"),
    ] {
        q!(
            format!(
                "g02_kind_{k}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE fql_kind = '{k}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 3: LIKE prefix scoped to small subtrees (10) ───────────────────
    for (p, path) in [
        ("uart%", "drivers/serial/**"),
        ("gpio%", "drivers/gpio/**"),
        ("i2c%", "drivers/i2c/**"),
        ("spi%", "drivers/spi/**"),
        ("bt%", "drivers/bluetooth/**"),
        ("k_%", "kernel/**"),
        ("z_%", "lib/**"),
        ("sys_%", "arch/**"),
        ("pm_%", "subsys/pm/**"),
        ("net_%", "subsys/net/**"),
    ] {
        q!(
            format!(
                "g03_like_{}_in_{}",
                p.replace('%', ""),
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE name LIKE '{p}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 4: Exact name + fql_kind combos (10) ────────────────────────────
    for (n, k) in [
        ("stopped", "field"),
        ("running", "field"),
        ("idle", "enum_variant"),
        ("init", "function"),
        ("reset", "function"),
        ("enable", "function"),
        ("disable", "function"),
        ("open", "function"),
        ("close", "function"),
        ("write", "function"),
    ] {
        q!(
            format!("g04_name_{n}_kind_{k}"),
            format!("FIND symbols WHERE name = '{n}' WHERE fql_kind = '{k}'")
        );
    }

    // ── Group 5: fql_kind + has_doc in scoped path (10) ──────────────────────
    for (k, f, val, path) in [
        ("function", "has_doc", "true", "drivers/serial/**"),
        ("function", "has_doc", "false", "drivers/serial/**"),
        ("struct", "has_doc", "true", "drivers/serial/**"),
        ("function", "has_doc", "true", "drivers/gpio/**"),
        ("function", "has_doc", "false", "drivers/gpio/**"),
        ("struct", "has_doc", "true", "drivers/gpio/**"),
        ("function", "has_doc", "true", "drivers/i2c/**"),
        ("function", "has_doc", "false", "drivers/i2c/**"),
        ("function", "has_doc", "true", "drivers/spi/**"),
        ("function", "has_doc", "false", "drivers/spi/**"),
    ] {
        q!(
            format!(
                "g05_{k}_{f}_{val}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE {f} = '{val}' ORDER BY name ASC IN '{path}'"
            )
        );
    }

    // ── Group 6: is_recursive + scoped path (6) ──────────────────────────────
    for (val, path) in [
        ("true", "drivers/serial/**"),
        ("false", "drivers/serial/**"),
        ("true", "drivers/gpio/**"),
        ("false", "drivers/gpio/**"),
        ("true", "kernel/**"),
        ("false", "drivers/i2c/**"),
    ] {
        q!(
            format!(
                "g06_is_recursive_{val}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE is_recursive = '{val}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 7: has_fallthrough scoped (4) ───────────────────────────────────
    for (val, path) in [
        ("true", "drivers/**"),
        ("false", "drivers/serial/**"),
        ("true", "kernel/**"),
        ("false", "drivers/gpio/**"),
    ] {
        q!(
            format!(
                "g07_has_fallthrough_{val}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE has_fallthrough = '{val}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 8: line range scoped to small files/dirs (8) ───────────────────
    for (pred, label, path) in [
        ("line < 20", "lt20", "drivers/serial/**"),
        ("line < 30", "lt30", "drivers/gpio/**"),
        ("line >= 100", "ge100", "drivers/serial/**"),
        ("line >= 200", "ge200", "drivers/gpio/**"),
        ("line < 20", "lt20", "drivers/i2c/**"),
        ("line >= 100", "ge100", "drivers/i2c/**"),
        ("line < 20", "lt20", "drivers/spi/**"),
        ("line >= 50", "ge50", "drivers/spi/**"),
    ] {
        q!(
            format!(
                "g08_line_{label}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE {pred} ORDER BY line ASC IN '{path}'")
        );
    }

    // ── Group 9: usages predicates scoped (8) ────────────────────────────────
    for (pred, label, path) in [
        ("usages > 50", "gt50", "drivers/serial/**"),
        ("usages > 10", "gt10", "drivers/gpio/**"),
        ("usages > 5", "gt5", "drivers/i2c/**"),
        ("usages > 20", "gt20", "drivers/spi/**"),
        ("usages = 0", "eq0", "drivers/serial/**"),
        ("usages = 0", "eq0", "drivers/gpio/**"),
        ("usages >= 50", "ge50", "drivers/bluetooth/**"),
        ("usages > 30", "gt30", "kernel/**"),
    ] {
        q!(
            format!(
                "g09_usages_{label}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE {pred} ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 10: ORDER BY + path scope (10) ─────────────────────────────────
    for (k, f, dir, path) in [
        ("function", "name", "ASC", "drivers/serial/**"),
        ("function", "name", "DESC", "drivers/serial/**"),
        ("function", "line", "ASC", "drivers/gpio/**"),
        ("struct", "name", "ASC", "drivers/serial/**"),
        ("enum", "name", "ASC", "drivers/serial/**"),
        ("function", "usages", "DESC", "drivers/serial/**"),
        ("function", "name", "ASC", "drivers/i2c/**"),
        ("function", "line", "ASC", "drivers/spi/**"),
        ("struct", "name", "ASC", "drivers/gpio/**"),
        ("function", "usages", "DESC", "drivers/gpio/**"),
    ] {
        q!(
            format!(
                "g10_{k}_order_{f}_{}_in_{}",
                dir.to_lowercase(),
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE fql_kind = '{k}' ORDER BY {f} {dir} IN '{path}'")
        );
    }

    // ── Group 11: LIKE + ORDER BY + path (8) ──────────────────────────────────
    for (p, f, dir, path) in [
        ("uart%", "name", "ASC", "drivers/serial/**"),
        ("gpio%", "line", "ASC", "drivers/gpio/**"),
        ("i2c%", "name", "ASC", "drivers/i2c/**"),
        ("spi%", "line", "ASC", "drivers/spi/**"),
        ("k_%", "name", "ASC", "kernel/**"),
        ("sys_%", "line", "ASC", "arch/**"),
        ("pm_%", "name", "ASC", "subsys/pm/**"),
        ("net_%", "name", "DESC", "subsys/net/**"),
    ] {
        q!(
            format!(
                "g11_like_{}_order_{f}_{}_in_{}",
                p.replace('%', ""),
                dir.to_lowercase(),
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE name LIKE '{p}' ORDER BY {f} {dir} IN '{path}'")
        );
    }

    // ── Group 12: kind + LIKE + enrichment + path (10) ───────────────────────
    for (k, p, f, val, path) in [
        ("function", "uart%", "has_doc", "true", "drivers/serial/**"),
        ("function", "uart%", "has_doc", "false", "drivers/serial/**"),
        ("function", "gpio%", "has_doc", "true", "drivers/gpio/**"),
        ("function", "gpio%", "has_doc", "false", "drivers/gpio/**"),
        ("function", "i2c%", "has_doc", "true", "drivers/i2c/**"),
        ("function", "spi%", "has_doc", "true", "drivers/spi/**"),
        ("function", "k_%", "has_doc", "true", "kernel/**"),
        ("function", "pm_%", "has_doc", "true", "subsys/pm/**"),
        ("function", "net_%", "has_doc", "true", "subsys/net/**"),
        ("struct", "uart%", "has_doc", "true", "drivers/serial/**"),
    ] {
        q!(
            format!(
                "g12_{k}_like_{}_{}_{}_in_{}",
                p.replace('%', ""),
                f,
                val,
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE name LIKE '{p}' WHERE {f} = '{val}' ORDER BY name ASC IN '{path}'"
            )
        );
    }

    // ── Group 13: NOT LIKE scoped (6) ─────────────────────────────────────────
    for (k, p, path) in [
        ("function", "uart%", "drivers/serial/**"),
        ("function", "gpio%", "drivers/gpio/**"),
        ("function", "i2c%", "drivers/i2c/**"),
        ("struct", "uart%", "drivers/serial/**"),
        ("function", "k_%", "kernel/**"),
        ("function", "zzz%", "drivers/serial/**"), // nothing matches → full set
    ] {
        q!(
            format!(
                "g13_{k}_not_like_{}_in_{}",
                p.replace('%', ""),
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE name NOT LIKE '{p}' ORDER BY name ASC IN '{path}'"
            )
        );
    }

    // ── Group 14: usages + kind + ORDER BY (8) ────────────────────────────────
    for (k, pred, label, ord, path) in [
        (
            "function",
            "usages > 50",
            "gt50",
            "usages",
            "drivers/serial/**",
        ),
        ("function", "usages > 10", "gt10", "name", "drivers/gpio/**"),
        ("struct", "usages > 5", "gt5", "name", "drivers/**"),
        (
            "function",
            "usages > 20",
            "gt20",
            "usages",
            "drivers/spi/**",
        ),
        ("function", "usages > 5", "gt5", "name", "drivers/i2c/**"),
        ("function", "usages = 0", "eq0", "name", "drivers/serial/**"),
        ("function", "usages = 0", "eq0", "line", "drivers/gpio/**"),
        ("struct", "usages = 0", "eq0", "name", "drivers/serial/**"),
    ] {
        q!(
            format!(
                "g14_{k}_usages_{label}_order_{ord}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE {pred} ORDER BY {ord} ASC IN '{path}'"
            )
        );
    }

    // ── Group 15: Clearly empty queries (5) ───────────────────────────────────
    q!(
        "g15_empty_name_xyz",
        "FIND symbols WHERE name = 'nonexistent_xyz_abc_123'"
    );
    q!(
        "g15_empty_kind_xyz",
        "FIND symbols WHERE fql_kind = 'nonexistent_kind_xyz'"
    );
    q!(
        "g15_empty_like_zzz",
        "FIND symbols WHERE name LIKE 'zzz_nomatch_%'"
    );
    q!("g15_empty_line_gt99999", "FIND symbols WHERE line > 99999");
    q!("g15_empty_line_lt0", "FIND symbols WHERE line < 0");

    // ── Group 16: Case-sensitivity regression (4) ─────────────────────────────
    // Tests that = is exact and LIKE is case-insensitive for both backends.
    q!("g16_case_eq_lower", "FIND symbols WHERE name = 'stopped'");
    q!("g16_case_eq_mixed", "FIND symbols WHERE name = 'Stopped'"); // → 0 results
    q!(
        "g16_case_like_lower",
        "FIND symbols WHERE name LIKE 'stopped'"
    );
    q!(
        "g16_case_like_mixed",
        "FIND symbols WHERE name LIKE 'Stopped'"
    ); // → same as lower

    // ── Group 17: LIMIT correctness (8) ──────────────────────────────────────
    // Use tight path-scoped predicates that return >LIMIT total results so the
    // LIMIT actually truncates.  Both backends must return the same count.
    for (lim, k, path) in [
        (3usize, "function", "drivers/serial/**"),
        (5, "function", "drivers/gpio/**"),
        (3, "struct", "drivers/serial/**"),
        (5, "function", "drivers/i2c/**"),
        (3, "function", "drivers/spi/**"),
        (5, "function", "kernel/**"),
        (3, "struct", "drivers/gpio/**"),
        (5, "enum", "drivers/**"),
    ] {
        q!(
            format!(
                "g17_limit{lim}_{k}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' ORDER BY name ASC LIMIT {lim} IN '{path}'"
            )
        );
    }

    // ── Group 18: OFFSET correctness (6) ──────────────────────────────────────
    // Small LIMIT + varying OFFSET on a stable ORDER BY to verify pagination
    // parity.
    for off in [0usize, 3, 5] {
        for (k, path) in [
            ("function", "drivers/serial/**"),
            ("function", "drivers/gpio/**"),
        ] {
            q!(
                format!(
                    "g18_off{off}_{k}_in_{}",
                    path.replace('/', "_").replace('*', "")
                ),
                format!(
                    "FIND symbols WHERE fql_kind = '{k}' ORDER BY name ASC LIMIT 5 OFFSET {off} IN '{path}'"
                )
            );
        }
    }

    // ── Group 19: Multi-enrichment combos scoped (8) ──────────────────────────
    for (f1, v1, f2, v2, path) in [
        (
            "has_doc",
            "true",
            "is_recursive",
            "true",
            "drivers/serial/**",
        ),
        (
            "has_doc",
            "true",
            "is_recursive",
            "false",
            "drivers/serial/**",
        ),
        (
            "has_doc",
            "false",
            "is_recursive",
            "false",
            "drivers/gpio/**",
        ),
        ("has_doc", "true", "is_recursive", "true", "drivers/gpio/**"),
        (
            "has_doc",
            "true",
            "has_fallthrough",
            "false",
            "drivers/serial/**",
        ),
        (
            "has_doc",
            "false",
            "has_fallthrough",
            "false",
            "drivers/gpio/**",
        ),
        ("has_doc", "true", "is_recursive", "true", "drivers/i2c/**"),
        ("has_doc", "true", "is_recursive", "false", "drivers/i2c/**"),
    ] {
        q!(
            format!(
                "g19_{f1}_{v1}_{f2}_{v2}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE {f1} = '{v1}' WHERE {f2} = '{v2}' ORDER BY name ASC IN '{path}'"
            )
        );
    }

    // ── Group 20: LIKE suffix/contains scoped (8) ─────────────────────────────
    for (p, path) in [
        ("%_init", "drivers/serial/**"),
        ("%_enable", "drivers/gpio/**"),
        ("%_write", "drivers/i2c/**"),
        ("%_read", "drivers/spi/**"),
        ("%_config", "drivers/serial/**"),
        ("%_get", "drivers/gpio/**"),
        ("%_set", "drivers/i2c/**"),
        ("%_handler", "kernel/**"),
    ] {
        q!(
            format!(
                "g20_like_{}_in_{}",
                p.replace(['%', '_'], "").trim_start_matches('_').to_owned() + "_suffix",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE name LIKE '{p}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 21: fql_kind != scoped (6) ──────────────────────────────────────
    for (k, path) in [
        ("function", "drivers/serial/**"),
        ("struct", "drivers/serial/**"),
        ("enum", "drivers/gpio/**"),
        ("variable", "drivers/i2c/**"),
        ("function", "drivers/spi/**"),
        ("constant", "drivers/bluetooth/**"),
    ] {
        q!(
            format!(
                "g21_kind_ne_{k}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE fql_kind != '{k}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 22: name != scoped (6) ──────────────────────────────────────────
    for (n, path) in [
        ("init", "drivers/serial/**"),
        ("enable", "drivers/gpio/**"),
        ("open", "drivers/i2c/**"),
        ("write", "drivers/spi/**"),
        ("send", "drivers/bluetooth/**"),
        ("read", "kernel/**"),
    ] {
        q!(
            format!(
                "g22_name_ne_{n}_in_{}",
                path.replace('/', "_").replace('*', "")
            ),
            format!("FIND symbols WHERE name != '{n}' ORDER BY name ASC IN '{path}'")
        );
    }

    // ── Group 23: Triple combo + ORDER BY (8) ─────────────────────────────────
    for (k, p, f, val, ord, path) in [
        (
            "function",
            "uart%",
            "has_doc",
            "true",
            "line",
            "drivers/serial/**",
        ),
        (
            "function",
            "uart%",
            "has_doc",
            "false",
            "name",
            "drivers/serial/**",
        ),
        (
            "function",
            "gpio%",
            "has_doc",
            "true",
            "name",
            "drivers/gpio/**",
        ),
        (
            "function",
            "gpio%",
            "is_recursive",
            "false",
            "line",
            "drivers/gpio/**",
        ),
        (
            "function",
            "i2c%",
            "has_doc",
            "true",
            "name",
            "drivers/i2c/**",
        ),
        (
            "function",
            "spi%",
            "has_doc",
            "true",
            "line",
            "drivers/spi/**",
        ),
        (
            "function",
            "k_%",
            "is_recursive",
            "false",
            "name",
            "kernel/**",
        ),
        (
            "function",
            "pm_%",
            "has_doc",
            "true",
            "name",
            "subsys/pm/**",
        ),
    ] {
        q!(
            format!(
                "g23_{k}_like_{}_{}_{}_order_{}_in_{}",
                p.replace('%', ""),
                f,
                val,
                ord,
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE fql_kind = '{k}' WHERE name LIKE '{p}' WHERE {f} = '{val}' ORDER BY {ord} ASC IN '{path}'"
            )
        );
    }

    // ── Group 24: Exact name + ORDER BY (8) ───────────────────────────────────
    for (n, ord) in [
        ("stopped", "line"),
        ("running", "line"),
        ("init", "line"),
        ("enable", "line"),
        ("disable", "line"),
        ("open", "line"),
        ("write", "line"),
        ("read", "line"),
    ] {
        q!(
            format!("g24_name_{n}_order_{ord}_asc"),
            format!("FIND symbols WHERE name = '{n}' ORDER BY {ord} ASC")
        );
    }

    // ── Group 25: usages + LIKE + path (6) ────────────────────────────────────
    for (pred, label, p, path) in [
        ("usages > 10", "gt10", "uart%", "drivers/serial/**"),
        ("usages > 5", "gt5", "gpio%", "drivers/gpio/**"),
        ("usages > 5", "gt5", "i2c%", "drivers/i2c/**"),
        ("usages = 0", "eq0", "uart%", "drivers/serial/**"),
        ("usages = 0", "eq0", "gpio%", "drivers/gpio/**"),
        ("usages > 20", "gt20", "k_%", "kernel/**"),
    ] {
        q!(
            format!(
                "g25_usages_{label}_like_{}_in_{}",
                p.replace('%', ""),
                path.replace('/', "_").replace('*', "")
            ),
            format!(
                "FIND symbols WHERE {pred} WHERE name LIKE '{p}' ORDER BY name ASC IN '{path}'"
            )
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
