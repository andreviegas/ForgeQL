//! Data-driven golden harness (v2), isolated from `zephyr_golden.rs`.
//!
//! Each `tests/golden/*.json` suite becomes one libtest-mimic trial per case,
//! named `<suite>::<case>`, so:
//!   cargo test --test golden_test                  # all
//!   cargo test --test golden_test enrich_is_magic  # one suite (group)
//!   cargo test --test golden_test enrich_is_magic::rust   # one case
//!
//! Setup/teardown: ONE server per process; `USE` is memoized per source.branch
//! (read-only — no BEGIN TRANSACTION), shared across every case. Parallel-safe
//! across agents (unique per-pid aliases) and across trials (Mutex-guarded stdio).
//! Requires FORGEQL_DATA_DIR; skips cleanly when unset.

// Test harness (harness = false): `main`-driven, so clippy's in-test relaxations
// don't apply. Allow the restriction/pedantic lints that fire on plumbing code.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::struct_field_names,
    clippy::option_if_let_else,
    clippy::collapsible_if,
    clippy::if_not_else
)]

use std::collections::{BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

use libtest_mimic::{Arguments, Failed, Trial};
use serde::Deserialize;
use serde_json::{Value, json};

// ───────────────────────── fixture schema ─────────────────────────
#[derive(Deserialize)]
struct Suite {
    suite: String,
    #[allow(dead_code)]
    #[serde(default)]
    description: String,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    #[serde(rename = "use")]
    use_str: String,
    /// "ro" (default) shares a memoized read-only session; "rw" gets a fresh
    /// per-case worktree off the corpus branch, discarded on teardown.
    #[serde(default = "mode_ro")]
    mode: String,
    /// One-shot read case: a single query + assert.
    #[serde(default)]
    fql: Option<String>,
    #[serde(default)]
    assert: Assert,
    /// Multi-step case (mutations / transactions): run in order in one session.
    #[serde(default)]
    steps: Vec<Step>,
}
fn mode_ro() -> String {
    "ro".to_string()
}

/// A step in a multi-step case. `${var}` placeholders in `fql` are substituted
/// from values `capture`d by earlier steps (keeps node_ids out of the fixture).
#[derive(Deserialize)]
struct Step {
    fql: String,
    #[serde(default)]
    assert: Assert,
    /// var name -> dotted path into this step's result JSON (e.g. "results.0.node_id").
    #[serde(default)]
    capture: std::collections::HashMap<String, String>,
}

#[derive(Deserialize, Default)]
struct Assert {
    #[serde(default)]
    row_count: Option<usize>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    all_same: Option<String>,
    #[serde(default)]
    same_block: Option<bool>,
    #[serde(default)]
    ordered: Option<Ordered>,
    #[serde(default)]
    distinct: Option<Distinct>,
    #[serde(default)]
    rows: Vec<Value>,
    // -- mutation / transaction result asserts --
    /// Expect the step's query to ERROR (e.g. ROLLBACK with no open txn).
    #[serde(default)]
    error: Option<bool>,
    /// Expect `result.applied == <bool>` (mutation result).
    #[serde(default)]
    applied: Option<bool>,
    /// Substring expected in `result.diff`.
    #[serde(default)]
    diff_contains: Option<String>,
    /// Exact `result.files_changed` array.
    #[serde(default)]
    files_changed: Option<Vec<Value>>,
    /// Top-level field equality, e.g. {"name": "inner"} for a rollback result.
    #[serde(default)]
    field: std::collections::HashMap<String, Value>,
    /// JSON-pointer equality, e.g. {"/results/0/line": 12}.
    #[serde(default)]
    pointer: std::collections::HashMap<String, Value>,
}
#[derive(Deserialize)]
struct Ordered {
    by: String,
    #[serde(default = "asc")]
    dir: String,
}
fn asc() -> String {
    "asc".to_string()
}
#[derive(Deserialize)]
struct Distinct {
    by: String,
    #[serde(default)]
    count: Option<usize>,
    #[serde(default)]
    values: Option<Vec<Value>>,
}

// ───────────────────────── MCP client ─────────────────────────
struct McpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    data_dir: PathBuf,
    created: Vec<(String, String)>,
}

impl McpClient {
    fn spawn(data_dir: &Path) -> std::io::Result<Self> {
        let binary = env!("CARGO_BIN_EXE_forgeql");
        let mut child = Command::new(binary)
            .arg("--mcp")
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--log-queries")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env("FORGEQL_SESSION_TTL_SECS", "3600")
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        let mut c = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            data_dir: data_dir.to_path_buf(),
            created: Vec::new(),
        };
        c.handshake()?;
        Ok(c)
    }
    fn handshake(&mut self) -> std::io::Result<()> {
        let init = self.request(
            "initialize",
            &json!({
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "golden_test", "version": "1.0"} }),
        )?;
        if init.get("error").is_some() {
            return Err(std::io::Error::other(format!("initialize failed: {init}")));
        }
        self.notify("notifications/initialized", &json!({}))
    }
    fn send_line(&mut self, msg: &Value) -> std::io::Result<()> {
        self.stdin.write_all(format!("{msg}\n").as_bytes())?;
        self.stdin.flush()
    }
    fn notify(&mut self, method: &str, params: &Value) -> std::io::Result<()> {
        self.send_line(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }
    fn read_line(&mut self) -> std::io::Result<Value> {
        let mut buf = String::new();
        loop {
            buf.clear();
            if self.stdout.read_line(&mut buf)? == 0 {
                return Err(std::io::Error::other("server closed stdout"));
            }
            let t = buf.trim();
            if t.is_empty() {
                continue;
            }
            return serde_json::from_str(t)
                .map_err(|e| std::io::Error::other(format!("json parse: {e} (line: {t})")));
        }
    }
    fn request(&mut self, method: &str, params: &Value) -> std::io::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send_line(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))?;
        loop {
            let resp = self.read_line()?;
            if resp.get("id").and_then(Value::as_u64) == Some(id) {
                return Ok(resp);
            }
        }
    }
    fn run_fql(&mut self, session_id: Option<&str>, fql: &str) -> std::io::Result<Value> {
        let mut args = json!({ "fql": fql, "format": "JSON" });
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
        for (use_str, alias) in std::mem::take(&mut self.created) {
            if let Some((source, branch)) = use_str.split_once('.') {
                forgeql_core::session::SessionCoords::new("anonymous", source, branch, &alias)
                    .teardown(&self.data_dir);
            }
        }
    }
}

// ───────────────────────── shared harness (setup/teardown) ─────────────────────────
struct Harness {
    client: McpClient,
    sessions: HashMap<String, String>,
    pid: u32,
}

impl Harness {
    fn new(data_dir: &Path) -> Self {
        let client = McpClient::spawn(data_dir).expect("spawn MCP server");
        Self {
            client,
            sessions: HashMap::new(),
            pid: std::process::id(),
        }
    }
    fn session_for(&mut self, use_str: &str) -> Result<String, String> {
        if let Some(s) = self.sessions.get(use_str) {
            return Ok(s.clone());
        }
        let alias = format!("gt-{}-{}", self.pid, self.sessions.len());
        let fql = format!("USE {use_str} AS '{alias}'");
        let res = self
            .client
            .run_fql(None, &fql)
            .map_err(|e| format!("{fql}: {e}"))?;
        self.client
            .created
            .push((use_str.to_string(), alias.clone()));
        let sid = res
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(&alias)
            .to_string();
        let _ = self.sessions.insert(use_str.to_string(), sid.clone());
        Ok(sid)
    }
    /// Fresh read-write session: a unique alias each call → its own worktree off
    /// the corpus branch, tracked for teardown. Used for mutation/transaction cases
    /// so each rw case is fully isolated and discarded when the run ends.
    fn rw_session(&mut self, use_str: &str) -> Result<String, String> {
        let alias = format!("gt-{}-rw-{}", self.pid, self.client.created.len());
        let fql = format!("USE {use_str} AS '{alias}'");
        let res = self
            .client
            .run_fql(None, &fql)
            .map_err(|e| format!("{fql}: {e}"))?;
        self.client
            .created
            .push((use_str.to_string(), alias.clone()));
        Ok(res
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or(&alias)
            .to_string())
    }

    /// Run a query on an explicit session id (used by multi-step cases).
    fn run(&mut self, sid: &str, fql: &str) -> Result<Value, String> {
        self.client
            .run_fql(Some(sid), fql)
            .map_err(|e| e.to_string())
    }
}

// ───────────────────────── node_id-aware derived fields ─────────────────────────
fn derived(row: &Value, key: &str) -> Value {
    let nid = row
        .get("node_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    match key {
        "_file" => match nid {
            Some(id) => json!(id.split('.').next().unwrap_or(id)),
            None => row.get("path").cloned().unwrap_or(Value::Null),
        },
        "_block" => nid.map_or(Value::Null, |id| json!(id.split('(').next().unwrap_or(id))),
        "_ordinal" => nid
            .and_then(|id| id.split_once('.'))
            .map_or(Value::Null, |(_, rest)| {
                json!(rest.split('(').next().unwrap_or(rest))
            }),
        "_offset" => nid
            .and_then(|id| id.split_once('('))
            .map_or(Value::Null, |(_, rest)| json!(rest.trim_end_matches(')'))),
        _ => row.get(key).cloned().unwrap_or(Value::Null),
    }
}
fn as_num(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

// ───────────────────────── assertion engine ─────────────────────────
fn rows_of(result: &Value) -> Vec<Value> {
    result
        .get("results")
        .or_else(|| result.pointer("/content/files"))
        .or_else(|| result.pointer("/content/entries"))
        .or_else(|| result.pointer("/content/members"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[allow(clippy::too_many_lines)]
fn check(a: &Assert, result: &Value) -> Vec<String> {
    let mut f = Vec::new();
    let rows = rows_of(result);

    if let Some(n) = a.row_count {
        if rows.len() != n {
            f.push(format!("row_count: expected {n}, got {}", rows.len()));
        }
    }
    if let Some(t) = a.total {
        let got = result.get("total").and_then(Value::as_u64).unwrap_or(0);
        if got != t {
            f.push(format!("total: expected {t}, got {got}"));
        }
    }
    if let Some(field) = &a.all_same {
        let mut it = rows.iter().map(|r| derived(r, field));
        if let Some(first) = it.next() {
            if !it.all(|v| v == first) {
                f.push(format!("all_same[{field}]: values differ"));
            }
        }
    }
    if a.same_block == Some(true) {
        let blocks: Vec<Value> = rows.iter().map(|r| derived(r, "_block")).collect();
        if blocks.iter().any(Value::is_null) {
            f.push("same_block: some rows have no node_id (not block-addressable)".into());
        } else if blocks.windows(2).any(|w| w[0] != w[1]) {
            f.push("same_block: rows span multiple blocks".into());
        }
    }
    if let Some(o) = &a.ordered {
        if o.by == "_ordinal" {
            f.push("ordered.by '_ordinal' is not a source-order key — use 'line'".into());
        } else {
            let nums: Vec<i64> = rows
                .iter()
                .filter_map(|r| as_num(&derived(r, &o.by)))
                .collect();
            if nums.len() != rows.len() {
                f.push(format!("ordered[{}]: non-numeric values present", o.by));
            } else {
                let ok = if o.dir == "desc" {
                    nums.windows(2).all(|w| w[0] >= w[1])
                } else {
                    nums.windows(2).all(|w| w[0] <= w[1])
                };
                if !ok {
                    f.push(format!("ordered[{}]: not {} ({nums:?})", o.by, o.dir));
                }
            }
        }
    }
    if let Some(d) = &a.distinct {
        let set: BTreeSet<String> = rows.iter().map(|r| derived(r, &d.by).to_string()).collect();
        if let Some(c) = d.count {
            if set.len() != c {
                f.push(format!(
                    "distinct[{}]: expected {c}, got {}",
                    d.by,
                    set.len()
                ));
            }
        }
        if let Some(vals) = &d.values {
            let exp: BTreeSet<String> = vals.iter().map(Value::to_string).collect();
            if set != exp {
                f.push(format!(
                    "distinct[{}] values: expected {exp:?}, got {set:?}",
                    d.by
                ));
            }
        }
    }
    for (i, exp) in a.rows.iter().enumerate() {
        let Some(obj) = exp.as_object() else { continue };
        let Some(actual) = rows.get(i) else {
            f.push(format!("row[{i}] missing (only {} rows)", rows.len()));
            continue;
        };
        for (k, ev) in obj {
            let av = actual.get(k).unwrap_or(&Value::Null);
            if av != ev {
                f.push(format!("row[{i}].{k}: expected {ev}, got {av}"));
            }
        }
    }
    if let Some(expected) = a.applied {
        let got = result
            .get("applied")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if got != expected {
            f.push(format!("applied: expected {expected}, got {got}"));
        }
    }
    if let Some(sub) = &a.diff_contains {
        let diff = result.get("diff").and_then(Value::as_str).unwrap_or("");
        if !diff.contains(sub) {
            f.push(format!("diff_contains: {sub:?} not found in diff"));
        }
    }
    if let Some(expected) = &a.files_changed {
        let got = result.get("files_changed").cloned().unwrap_or(Value::Null);
        if got.as_array() != Some(expected) {
            f.push(format!("files_changed: expected {expected:?}, got {got}"));
        }
    }
    for (k, ev) in &a.field {
        let av = result.get(k).unwrap_or(&Value::Null);
        if av != ev {
            f.push(format!("field[{k}]: expected {ev}, got {av}"));
        }
    }
    for (ptr, ev) in &a.pointer {
        let av = result.pointer(ptr).unwrap_or(&Value::Null);
        if av != ev {
            f.push(format!("pointer[{ptr}]: expected {ev}, got {av}"));
        }
    }
    f
}

/// Substitute `${var}` placeholders in a step's fql from captured values.
fn interpolate(fql: &str, vars: &HashMap<String, String>) -> String {
    let mut out = fql.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("${{{k}}}"), v);
    }
    out
}

/// Extract a dotted path (e.g. "results.0.node_id") from a result Value.
fn extract_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for seg in path.split('.') {
        cur = match seg.parse::<usize>() {
            Ok(idx) => cur.get(idx)?,
            Err(_) => cur.get(seg)?,
        };
    }
    Some(cur)
}

/// Run one case — single-shot read, or a multi-step (mutation/transaction) case.
fn run_case(h: &Arc<Mutex<Harness>>, case: &Case) -> Result<(), Failed> {
    // Resolve a session: rw → a fresh isolated worktree; ro → shared memoized.
    let sid = {
        let mut hg = h.lock().unwrap();
        if case.mode == "rw" {
            hg.rw_session(&case.use_str)
        } else {
            hg.session_for(&case.use_str)
        }
    }
    .map_err(Failed::from)?;

    // One-shot read case: a single query + assert.
    if case.steps.is_empty() {
        let fql = case
            .fql
            .as_deref()
            .ok_or_else(|| Failed::from("case has neither 'fql' nor 'steps'"))?;
        let result = { h.lock().unwrap().run(&sid, fql) }.map_err(Failed::from)?;
        let fails = check(&case.assert, &result);
        return if fails.is_empty() {
            Ok(())
        } else {
            Err(Failed::from(fails.join("; ")))
        };
    }

    // Multi-step case: one shared session, captures threaded across steps.
    let mut vars: HashMap<String, String> = HashMap::new();
    for (i, step) in case.steps.iter().enumerate() {
        let fql = interpolate(&step.fql, &vars);
        let outcome = { h.lock().unwrap().run(&sid, &fql) };

        // `error: true` — the step is expected to fail (e.g. ROLLBACK with no txn).
        if step.assert.error == Some(true) {
            if outcome.is_ok() {
                return Err(Failed::from(format!(
                    "step[{i}]: expected an error, but the query succeeded"
                )));
            }
            continue;
        }
        let result = outcome.map_err(|e| Failed::from(format!("step[{i}] '{fql}': {e}")))?;

        // Capture values for later `${var}` interpolation.
        for (var, path) in &step.capture {
            let v = extract_path(&result, path).ok_or_else(|| {
                Failed::from(format!("step[{i}]: capture path '{path}' not found"))
            })?;
            let s = v.as_str().map_or_else(|| v.to_string(), str::to_string);
            let _ = vars.insert(var.clone(), s);
        }

        let fails = check(&step.assert, &result);
        if !fails.is_empty() {
            return Err(Failed::from(format!("step[{i}]: {}", fails.join("; "))));
        }
    }
    Ok(())
}

// ───────────────────────── entrypoint ─────────────────────────
fn main() {
    let args = Arguments::from_args();

    let Ok(dir) = std::env::var("FORGEQL_DATA_DIR") else {
        eprintln!("[golden] SKIP — FORGEQL_DATA_DIR not set");
        return;
    };
    let data_dir = PathBuf::from(dir);
    let golden_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("golden");

    let mut suites: Vec<Suite> = Vec::new();
    for entry in std::fs::read_dir(&golden_dir)
        .expect("read tests/golden")
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let txt = std::fs::read_to_string(&path).expect("read suite");
            suites.push(
                serde_json::from_str(&txt)
                    .unwrap_or_else(|e| panic!("parse {}: {e}", path.display())),
            );
        }
    }

    let harness = Arc::new(Mutex::new(Harness::new(&data_dir)));
    let mut trials = Vec::new();
    for suite in suites {
        let sname = suite.suite;
        for case in suite.cases {
            let h = Arc::clone(&harness);
            let name = format!("{sname}::{}", case.name);
            trials.push(Trial::test(name, move || run_case(&h, &case)));
        }
    }

    let conclusion = libtest_mimic::run(&args, trials);
    drop(harness);
    conclusion.exit();
}
