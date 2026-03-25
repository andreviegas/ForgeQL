//! **SMS — State Machine Syntax** combinatorial test harness.
//!
//! Reads `tests/fixtures/syntax.json` and generates FQL commands that cover
//! every (command × clause × operator) combination.  Two-phase generation:
//!
//! - **Phase 1 (baseline):** one command per (command\_id, clause) pair so
//!   every command type and every clause appears at least once.
//! - **Phase 2 (random fill):** seeded deterministic random combinations
//!   fill the remaining budget up to `total_budget`, respecting per-command
//!   `explosion_cap`.
//!
//! Three assertion tiers:
//! - Tier 1: parse without error.
//! - Tier 2: execute without panic (errors like "symbol not found" are OK).
//! - Tier 3: invariant checking on successful results (LIMIT, IN, ORDER BY …).
//!
//! Run with: `cargo test -p forgeql-core --test sms_integration -- --nocapture`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::missing_const_for_fn,
    clippy::cast_possible_truncation,
    clippy::single_match,
    clippy::collapsible_if,
    clippy::manual_let_else,
    clippy::map_unwrap_or,
    clippy::doc_markdown,
    clippy::used_underscore_binding
)]

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry, RustLanguageInline};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, ShowContent};
use serde_json::Value;
use tempfile::tempdir;

// -----------------------------------------------------------------------
// Deterministic PRNG (xorshift64) — no external dependency
// -----------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_usize(&mut self, bound: usize) -> usize {
        if bound == 0 {
            return 0;
        }
        (self.next_u64() % bound as u64) as usize
    }

    fn pick<'a, T>(&mut self, slice: &'a [T]) -> &'a T {
        &slice[self.next_usize(slice.len())]
    }

    fn chance(&mut self, pct: u64) -> bool {
        self.next_u64() % 100 < pct
    }
}

// -----------------------------------------------------------------------
// syntax.json model (lightweight — extract only what we need)
// -----------------------------------------------------------------------

struct SyntaxSpec {
    seed: u64,
    total_budget: usize,
    commands: Vec<CommandSpec>,
    clauses: HashMap<String, ClauseSpec>,
    operators: Vec<OpSpec>,
    result_types: HashMap<String, ResultTypeSpec>,
    test_values: HashMap<String, Vec<String>>,
}

struct CommandSpec {
    id: String,
    syntax: String,
    category: String,
    result_type: Option<String>,
    required_args: Vec<ArgSpec>,
    optional_args: Vec<OptArgSpec>,
    clauses: Vec<String>,
    explosion_cap: usize,
}

struct ArgSpec {
    name: String,
    pool: String,
}

struct OptArgSpec {
    _name: String,
    syntax: String,
    pool: String,
}

struct ClauseSpec {
    _syntax: String,
    can_repeat: usize,
    _value_pool: Option<String>,
    _field_source: Option<String>,
    directions: Option<Vec<String>>,
    applies_to: Option<Vec<String>>,
}

struct OpSpec {
    syntax: String,
    value_types: Vec<String>,
}

struct ResultTypeSpec {
    string_fields: Vec<String>,
    numeric_fields: Vec<String>,
    accepts_enrichment: bool,
}

fn load_syntax() -> SyntaxSpec {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/syntax.json");
    let text = fs::read_to_string(&path).expect("read syntax.json");
    let root: Value = serde_json::from_str(&text).expect("parse syntax.json");

    let meta = &root["meta"];
    let seed = meta["seed"].as_u64().unwrap_or(42);
    let total_budget = meta["total_budget"].as_u64().unwrap_or(200) as usize;

    let operators: Vec<OpSpec> = root["operators"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| OpSpec {
            syntax: v["syntax"].as_str().unwrap().to_string(),
            value_types: v["value_types"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect(),
        })
        .collect();

    let result_types: HashMap<String, ResultTypeSpec> = root["result_types"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| {
            (
                k.clone(),
                ResultTypeSpec {
                    string_fields: arr_str(&v["string_fields"]),
                    numeric_fields: arr_str(&v["numeric_fields"]),
                    accepts_enrichment: v["accepts_enrichment"].as_bool().unwrap_or(false),
                },
            )
        })
        .collect();

    let mut clauses_map: HashMap<String, ClauseSpec> = HashMap::new();
    for (k, v) in root["clauses"].as_object().unwrap() {
        let _ = clauses_map.insert(
            k.clone(),
            ClauseSpec {
                _syntax: v["syntax"].as_str().unwrap().to_string(),
                can_repeat: v["can_repeat"].as_u64().unwrap_or(0) as usize,
                _value_pool: v
                    .get("value_pool")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                _field_source: v
                    .get("field_source")
                    .and_then(|x| x.as_str())
                    .map(String::from),
                directions: v
                    .get("directions")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect()),
                applies_to: v
                    .get("applies_to")
                    .and_then(|x| x.as_array())
                    .map(|a| a.iter().map(|x| x.as_str().unwrap().to_string()).collect()),
            },
        );
    }

    let commands: Vec<CommandSpec> = root["commands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| CommandSpec {
            id: v["id"].as_str().unwrap().to_string(),
            syntax: v["syntax"].as_str().unwrap().to_string(),
            category: v["category"].as_str().unwrap().to_string(),
            result_type: v["result_type"].as_str().map(String::from),
            required_args: v["required_args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| ArgSpec {
                    name: a["name"].as_str().unwrap().to_string(),
                    pool: a["pool"].as_str().unwrap().to_string(),
                })
                .collect(),
            optional_args: v
                .get("optional_args")
                .and_then(|x| x.as_array())
                .unwrap_or(&vec![])
                .iter()
                .map(|a| OptArgSpec {
                    _name: a["name"].as_str().unwrap().to_string(),
                    syntax: a["syntax"].as_str().unwrap().to_string(),
                    pool: a["pool"].as_str().unwrap().to_string(),
                })
                .collect(),
            clauses: v["clauses"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x.as_str().unwrap().to_string())
                .collect(),
            explosion_cap: v["explosion_cap"].as_u64().unwrap_or(100) as usize,
        })
        .collect();

    let mut test_values: HashMap<String, Vec<String>> = HashMap::new();
    for (k, v) in root["test_values"].as_object().unwrap() {
        if k == "description" {
            continue;
        }
        match v {
            Value::Array(arr) => {
                let _ = test_values.insert(
                    k.clone(),
                    arr.iter()
                        .map(|x| match x {
                            Value::String(s) => s.clone(),
                            Value::Number(n) => n.to_string(),
                            Value::Bool(b) => b.to_string(),
                            _ => x.to_string(),
                        })
                        .collect(),
                );
            }
            _ => {}
        }
    }

    SyntaxSpec {
        seed,
        total_budget,
        commands,
        clauses: clauses_map,
        operators,
        result_types,
        test_values,
    }
}

fn arr_str(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect()
}

// -----------------------------------------------------------------------
// Command generation
// -----------------------------------------------------------------------

struct GeneratedCommand {
    fql: String,
    command_id: String,
    _phase: u8, // 1 or 2
}

fn pool_value(rng: &mut Rng, pool_name: &str, tv: &HashMap<String, Vec<String>>) -> String {
    tv.get(pool_name)
        .filter(|v| !v.is_empty())
        .map(|v| rng.pick(v).clone())
        .unwrap_or_else(|| "foo".to_string())
}

/// Build the base command string (no clauses) by expanding the syntax template
/// with concrete values from test_values pools.
fn build_base(cmd: &CommandSpec, rng: &mut Rng, tv: &HashMap<String, Vec<String>>) -> String {
    let mut s = cmd.syntax.clone();
    for arg in &cmd.required_args {
        let val = pool_value(rng, &arg.pool, tv);
        let placeholder = format!("{{{}}}", arg.name);
        s = s.replace(&placeholder, &val);
        // Also replace quoted placeholder: '{name}' → already in syntax
        let quoted_ph = format!("'{{{}}}'", arg.name);
        s = s.replace(&quoted_ph, &format!("'{val}'"));
    }
    s
}

/// Expand a single clause into FQL text.
fn expand_clause(
    clause_name: &str,
    spec: &ClauseSpec,
    cmd: &CommandSpec,
    rng: &mut Rng,
    tv: &HashMap<String, Vec<String>>,
    result_types: &HashMap<String, ResultTypeSpec>,
    operators: &[OpSpec],
) -> Option<String> {
    // Check applies_to restriction
    if let Some(ref targets) = spec.applies_to {
        if !targets.contains(&cmd.id) {
            return None;
        }
    }

    match clause_name {
        "WHERE" | "HAVING" => {
            let rt = cmd.result_type.as_ref().and_then(|k| result_types.get(k));
            let (field, is_numeric) = pick_field(rng, rt, clause_name == "WHERE");
            let op = pick_operator(rng, operators, is_numeric);
            let value = if is_numeric {
                pool_value(rng, "numeric_values", tv)
            } else {
                format!("'{}'", pool_value(rng, "string_values", tv))
            };
            Some(format!("{clause_name} {field} {op} {value}"))
        }
        "IN" | "EXCLUDE" => {
            let glob = pool_value(rng, "globs", tv);
            let keyword = if clause_name == "IN" { "IN" } else { "EXCLUDE" };
            Some(format!("{keyword} '{glob}'"))
        }
        "ORDER_BY" => {
            let rt = cmd.result_type.as_ref().and_then(|k| result_types.get(k));
            let (field, _) = pick_field(rng, rt, false);
            let dir = spec
                .directions
                .as_ref()
                .map(|d| rng.pick(d).as_str())
                .unwrap_or("DESC");
            Some(format!("ORDER BY {field} {dir}"))
        }
        "GROUP_BY" => {
            let rt = cmd.result_type.as_ref().and_then(|k| result_types.get(k));
            let field = rt
                .map(|r| rng.pick(&r.string_fields).clone())
                .unwrap_or_else(|| "name".to_string());
            Some(format!("GROUP BY {field}"))
        }
        "LIMIT" => {
            let val = pool_value(rng, "limit_values", tv);
            Some(format!("LIMIT {val}"))
        }
        "OFFSET" => {
            let val = pool_value(rng, "offset_values", tv);
            Some(format!("OFFSET {val}"))
        }
        "DEPTH" => {
            let val = pool_value(rng, "depth_values", tv);
            Some(format!("DEPTH {val}"))
        }
        "LINES" => {
            let val = pool_value(rng, "lines_values", tv);
            Some(format!("LINES {val}"))
        }
        _ => None,
    }
}

fn pick_field(
    rng: &mut Rng,
    rt: Option<&ResultTypeSpec>,
    include_enrichment: bool,
) -> (String, bool) {
    let rt = match rt {
        Some(r) => r,
        None => return ("name".to_string(), false),
    };
    let total_str = rt.string_fields.len();
    let total_num = rt.numeric_fields.len();
    let total = total_str + total_num;
    if total == 0 {
        return ("name".to_string(), false);
    }
    // For enrichment-capable types, sometimes pick an enrichment field
    if include_enrichment && rt.accepts_enrichment && rng.chance(30) {
        let enrich_fields = [
            ("has_doc", false),
            ("is_recursive", false),
            ("has_todo", false),
            ("lines", true),
            ("param_count", true),
            ("naming", false),
            ("scope", false),
            ("num_format", false),
        ];
        let (f, is_num) = rng.pick(&enrich_fields);
        return (f.to_string(), *is_num);
    }
    let idx = rng.next_usize(total);
    if idx < total_str {
        (rt.string_fields[idx].clone(), false)
    } else {
        (rt.numeric_fields[idx - total_str].clone(), true)
    }
}

fn pick_operator(rng: &mut Rng, operators: &[OpSpec], is_numeric: bool) -> String {
    let candidates: Vec<&OpSpec> = if is_numeric {
        operators
            .iter()
            .filter(|o| o.value_types.contains(&"number".to_string()))
            .collect()
    } else {
        operators
            .iter()
            .filter(|o| o.value_types.contains(&"string".to_string()))
            .collect()
    };
    if candidates.is_empty() {
        return "=".to_string();
    }
    rng.pick(&candidates).syntax.clone()
}

/// Phase 1: generate baseline commands — one per (command, clause) pair.
fn generate_baseline(spec: &SyntaxSpec) -> Vec<GeneratedCommand> {
    let mut rng = Rng::new(spec.seed);
    let mut cmds = Vec::new();

    for cmd in &spec.commands {
        // Bare command (no clauses)
        let base = build_base(cmd, &mut rng, &spec.test_values);
        cmds.push(GeneratedCommand {
            fql: base.clone(),
            command_id: cmd.id.clone(),
            _phase: 1,
        });

        // One command per supported clause
        for clause_name in &cmd.clauses {
            let clause_key = clause_name.as_str();
            if let Some(clause_spec) = spec.clauses.get(clause_key) {
                if let Some(clause_text) = expand_clause(
                    clause_key,
                    clause_spec,
                    cmd,
                    &mut rng,
                    &spec.test_values,
                    &spec.result_types,
                    &spec.operators,
                ) {
                    cmds.push(GeneratedCommand {
                        fql: format!("{base} {clause_text}"),
                        command_id: cmd.id.clone(),
                        _phase: 1,
                    });
                }
            }
        }

        // Include optional args variant
        if !cmd.optional_args.is_empty() {
            let mut opt_base = build_base(cmd, &mut rng, &spec.test_values);
            for opt in &cmd.optional_args {
                let val = pool_value(&mut rng, &opt.pool, &spec.test_values);
                let expanded = opt.syntax.replace(&format!("{{{}}}", opt._name), &val);
                let expanded =
                    expanded.replace(&format!("'{{{}}}'", opt._name), &format!("'{val}'"));
                opt_base = format!("{opt_base} {expanded}");
            }
            cmds.push(GeneratedCommand {
                fql: opt_base,
                command_id: cmd.id.clone(),
                _phase: 1,
            });
        }
    }

    cmds
}

/// Phase 2: fill remaining budget with random clause combinations.
fn generate_random_fill(spec: &SyntaxSpec, baseline_count: usize) -> Vec<GeneratedCommand> {
    let remaining = spec.total_budget.saturating_sub(baseline_count);
    if remaining == 0 {
        return Vec::new();
    }

    let mut rng = Rng::new(spec.seed.wrapping_mul(7919)); // different sequence from phase 1
    let mut cmds = Vec::new();
    let mut per_cmd_count: HashMap<String, usize> = HashMap::new();

    // Only generate for query/show commands (they have clauses)
    let eligible: Vec<&CommandSpec> = spec
        .commands
        .iter()
        .filter(|c| !c.clauses.is_empty())
        .collect();

    if eligible.is_empty() {
        return cmds;
    }

    for _ in 0..remaining * 3 {
        // try up to 3x budget to fill
        if cmds.len() >= remaining {
            break;
        }

        let cmd = rng.pick(&eligible);
        let count = per_cmd_count.get(&cmd.id).copied().unwrap_or(0);
        if count >= cmd.explosion_cap {
            continue;
        }

        let base = build_base(cmd, &mut rng, &spec.test_values);

        // Pick 1–3 random clauses
        let num_clauses = 1 + rng.next_usize(3.min(cmd.clauses.len()));
        let mut clause_parts = Vec::new();
        let mut used_clauses: HashMap<String, usize> = HashMap::new();

        for _ in 0..num_clauses {
            let clause_name = rng.pick(&cmd.clauses);
            let clause_key = clause_name.as_str();
            let already = used_clauses.get(clause_key).copied().unwrap_or(0);
            let max_repeat = spec
                .clauses
                .get(clause_key)
                .map(|c| if c.can_repeat == 0 { 1 } else { c.can_repeat })
                .unwrap_or(1);

            if already >= max_repeat {
                continue;
            }

            if let Some(clause_spec) = spec.clauses.get(clause_key) {
                if let Some(text) = expand_clause(
                    clause_key,
                    clause_spec,
                    cmd,
                    &mut rng,
                    &spec.test_values,
                    &spec.result_types,
                    &spec.operators,
                ) {
                    clause_parts.push(text);
                    *used_clauses.entry(clause_key.to_string()).or_default() += 1;
                }
            }
        }

        if clause_parts.is_empty() {
            continue;
        }

        let fql = format!("{base} {}", clause_parts.join(" "));
        *per_cmd_count.entry(cmd.id.clone()).or_default() += 1;
        cmds.push(GeneratedCommand {
            fql,
            command_id: cmd.id.clone(),
            _phase: 2,
        });
    }

    cmds
}

// -----------------------------------------------------------------------
// Engine setup (same pattern as multilang_resolve_integration.rs)
// -----------------------------------------------------------------------

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures/canonical")
}

fn make_registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![
        Arc::new(CppLanguageInline),
        Arc::new(RustLanguageInline),
    ]))
}

fn build_engine() -> (ForgeQLEngine, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();

    let _ =
        fs::copy(src.join("canonical.cpp"), dir.path().join("canonical.cpp")).expect("copy .cpp");
    let _ = fs::copy(src.join("canonical.rs"), dir.path().join("canonical.rs")).expect("copy .rs");

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    let session_id = engine
        .register_local_session(dir.path())
        .expect("register session");

    (engine, session_id, dir)
}

// -----------------------------------------------------------------------
// Assertion tiers
// -----------------------------------------------------------------------

struct TierResult {
    fql: String,
    command_id: String,
    tier: u8,
    duration_us: u128,
    status: &'static str,
    result_count: Option<usize>,
}

fn tier1_parse(cmd: &GeneratedCommand) -> TierResult {
    let start = Instant::now();
    let status = match parser::parse(&cmd.fql) {
        Ok(_) => "ok",
        Err(_) => "parse_error",
    };
    TierResult {
        fql: cmd.fql.clone(),
        command_id: cmd.command_id.clone(),
        tier: 1,
        duration_us: start.elapsed().as_micros(),
        status,
        result_count: None,
    }
}

fn tier2_no_crash(cmd: &GeneratedCommand, engine: &mut ForgeQLEngine, session: &str) -> TierResult {
    let ops = match parser::parse(&cmd.fql) {
        Ok(ops) => ops,
        Err(_) => {
            return TierResult {
                fql: cmd.fql.clone(),
                command_id: cmd.command_id.clone(),
                tier: 2,
                duration_us: 0,
                status: "skipped_parse_fail",
                result_count: None,
            };
        }
    };
    let op = match ops.first() {
        Some(op) => op,
        None => {
            return TierResult {
                fql: cmd.fql.clone(),
                command_id: cmd.command_id.clone(),
                tier: 2,
                duration_us: 0,
                status: "skipped_empty",
                result_count: None,
            };
        }
    };

    let start = Instant::now();
    // Catch panics — a panic is a real bug, but we record it
    // rather than letting the whole suite abort.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        engine.execute(Some(session), op)
    }));
    let elapsed = start.elapsed().as_micros();

    let (status, count) = match outcome {
        Ok(Ok(result)) => ("ok", result_count(&result)),
        Ok(Err(_)) => ("exec_error", None),
        Err(_) => ("panic", None),
    };
    TierResult {
        fql: cmd.fql.clone(),
        command_id: cmd.command_id.clone(),
        tier: 2,
        duration_us: elapsed,
        status,
        result_count: count,
    }
}

fn tier3_invariants(
    cmd: &GeneratedCommand,
    engine: &mut ForgeQLEngine,
    session: &str,
) -> Option<TierResult> {
    let ops = parser::parse(&cmd.fql).ok()?;
    let op = ops.first()?;
    let start = Instant::now();
    let result = engine.execute(Some(session), op).ok()?;
    let elapsed = start.elapsed().as_micros();

    // Check LIMIT invariant
    if let Some(limit) = extract_limit(&cmd.fql) {
        if let Some(count) = result_count(&result) {
            if count > limit {
                return Some(TierResult {
                    fql: cmd.fql.clone(),
                    command_id: cmd.command_id.clone(),
                    tier: 3,
                    duration_us: elapsed,
                    status: "invariant_fail_limit",
                    result_count: Some(count),
                });
            }
        }
    }

    // Check GROUP BY invariant (no duplicate values)
    if let Some(field) = extract_group_by(&cmd.fql) {
        if !check_group_by_unique(&result, &field) {
            return Some(TierResult {
                fql: cmd.fql.clone(),
                command_id: cmd.command_id.clone(),
                tier: 3,
                duration_us: elapsed,
                status: "invariant_fail_group_by",
                result_count: result_count(&result),
            });
        }
    }

    Some(TierResult {
        fql: cmd.fql.clone(),
        command_id: cmd.command_id.clone(),
        tier: 3,
        duration_us: elapsed,
        status: "ok",
        result_count: result_count(&result),
    })
}

/// Extract LIMIT value from FQL text.
fn extract_limit(fql: &str) -> Option<usize> {
    let upper = fql.to_uppercase();
    let idx = upper.find("LIMIT ")?;
    let after = &fql[idx + 6..];
    after.split_whitespace().next()?.parse().ok()
}

/// Extract GROUP BY field from FQL text.
fn extract_group_by(fql: &str) -> Option<String> {
    let upper = fql.to_uppercase();
    let idx = upper.find("GROUP BY ")?;
    let after = &fql[idx + 9..];
    Some(after.split_whitespace().next()?.to_string())
}

/// Check that GROUP BY produced unique values in the grouped field.
fn check_group_by_unique(result: &ForgeQLResult, field: &str) -> bool {
    match result {
        ForgeQLResult::Query(qr) => {
            let mut seen = std::collections::HashSet::new();
            for item in &qr.results {
                let value = match field {
                    "name" => Some(item.name.clone()),
                    "kind" | "node_kind" => item.node_kind.clone(),
                    "fql_kind" => item.fql_kind.clone(),
                    "language" | "lang" => item.language.clone(),
                    "path" | "file" => item.path.as_ref().map(|p| p.display().to_string()),
                    other => item.fields.get(other).cloned(),
                };
                if let Some(v) = value {
                    if !seen.insert(v) {
                        return false;
                    }
                }
            }
            true
        }
        _ => true,
    }
}

fn result_count(result: &ForgeQLResult) -> Option<usize> {
    match result {
        ForgeQLResult::Query(qr) => Some(qr.results.len()),
        ForgeQLResult::Show(sr) => match &sr.content {
            ShowContent::Lines { lines, .. } => Some(lines.len()),
            ShowContent::Outline { entries } => Some(entries.len()),
            ShowContent::Members { members, .. } => Some(members.len()),
            ShowContent::CallGraph { entries, .. } => Some(entries.len()),
            ShowContent::FileList { files, .. } => Some(files.len()),
            ShowContent::Signature { .. } => Some(1),
        },
        _ => None,
    }
}

// -----------------------------------------------------------------------
// CSV output
// -----------------------------------------------------------------------

fn write_csv(results: &[TierResult]) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/scripts/logs");
    let _ = fs::create_dir_all(&dir);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let path = dir.join(format!("sms-perf-{timestamp}.csv"));

    let mut f = fs::File::create(&path).expect("create csv");
    writeln!(
        f,
        "fql_command,command_id,tier,duration_us,result_status,result_count"
    )
    .unwrap();
    for r in results {
        let count_str = r.result_count.map(|c| c.to_string()).unwrap_or_default();
        // Escape the FQL command for CSV
        let escaped = r.fql.replace('"', "\"\"");
        writeln!(
            f,
            "\"{escaped}\",{},{},{},{},{}",
            r.command_id, r.tier, r.duration_us, r.status, count_str
        )
        .unwrap();
    }

    eprintln!("[SMS] Wrote {} rows to {}", results.len(), path.display());
}

// -----------------------------------------------------------------------
// The test
// -----------------------------------------------------------------------

#[test]
fn sms_combinatorial() {
    let spec = load_syntax();

    // Phase 1: baseline
    let baseline = generate_baseline(&spec);
    let baseline_count = baseline.len();

    // Phase 2: random fill
    let random = generate_random_fill(&spec, baseline_count);

    let all_commands: Vec<GeneratedCommand> = baseline.into_iter().chain(random).collect();

    eprintln!(
        "[SMS] Generated {} commands (phase1={}, phase2={})",
        all_commands.len(),
        baseline_count,
        all_commands.len() - baseline_count
    );

    // Skip session/mutation/transaction commands for execution tiers
    let skip_categories = ["session", "mutation", "transaction"];

    // --- Tier 1: parse ---
    let mut all_results = Vec::new();
    let mut parse_failures = Vec::new();

    for cmd in &all_commands {
        let r = tier1_parse(cmd);
        if r.status != "ok" {
            parse_failures.push(r.fql.clone());
        }
        all_results.push(r);
    }

    // --- Tier 2 + 3: execute (only query/show commands) ---
    let (mut engine, session_id, _dir) = build_engine();

    for cmd in &all_commands {
        // Determine category from syntax.json
        let cat = spec
            .commands
            .iter()
            .find(|c| c.id == cmd.command_id)
            .map(|c| c.category.as_str())
            .unwrap_or("unknown");

        if skip_categories.contains(&cat) {
            continue;
        }

        let r2 = tier2_no_crash(cmd, &mut engine, &session_id);
        let r2_ok = r2.status == "ok";
        all_results.push(r2);

        // Tier 3 only if tier 2 passed
        if r2_ok {
            if let Some(r3) = tier3_invariants(cmd, &mut engine, &session_id) {
                all_results.push(r3);
            }
        }
    }

    // --- Write CSV ---
    write_csv(&all_results);

    // --- Coverage report ---
    let unique_commands: std::collections::HashSet<&str> =
        all_commands.iter().map(|c| c.command_id.as_str()).collect();

    let unique_clauses: std::collections::HashSet<String> = all_commands
        .iter()
        .flat_map(|c| {
            let upper = c.fql.to_uppercase();
            let mut found = Vec::new();
            for kw in &[
                "WHERE", "HAVING", "IN ", "EXCLUDE", "ORDER BY", "GROUP BY", "LIMIT", "OFFSET",
                "DEPTH", "LINES ",
            ] {
                if upper.contains(kw) {
                    found.push(kw.trim().to_string());
                }
            }
            found
        })
        .collect();

    eprintln!("[SMS] Command types covered: {}", unique_commands.len());
    eprintln!("[SMS] Clause types covered:  {}", unique_clauses.len());
    eprintln!("[SMS] Parse failures:        {}", parse_failures.len());

    // --- Assertions ---
    // All commands must parse (tier 1)
    assert!(
        parse_failures.is_empty(),
        "[SMS] Parse failures:\n{}",
        parse_failures.join("\n")
    );

    // Must cover all command types that appear in syntax.json
    let expected_cmd_count = spec.commands.len();
    assert_eq!(
        unique_commands.len(),
        expected_cmd_count,
        "[SMS] Expected {expected_cmd_count} command types, got {}",
        unique_commands.len()
    );

    // Must cover at least 8 clause types (WHERE, IN, EXCLUDE, ORDER_BY, GROUP_BY, LIMIT, OFFSET, DEPTH)
    assert!(
        unique_clauses.len() >= 8,
        "[SMS] Expected at least 8 clause types, got {}: {:?}",
        unique_clauses.len(),
        unique_clauses
    );

    // No tier-3 invariant failures
    let invariant_failures: Vec<&TierResult> = all_results
        .iter()
        .filter(|r| r.tier == 3 && r.status != "ok")
        .collect();
    assert!(
        invariant_failures.is_empty(),
        "[SMS] Tier-3 invariant failures:\n{}",
        invariant_failures
            .iter()
            .map(|r| format!("  [{}] {} → {}", r.command_id, r.fql, r.status))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
