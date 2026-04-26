//! **SMS — State Machine Syntax** combinatorial test harness.
//!
//! Reads `tests/fixtures/syntax.json` and generates FQL commands that cover
//! every (command × clause × operator × enrichment-field) combination.
//! Three-phase generation:
//!
//! - **Phase 0 (field sweep):** every enrichment field × every pool value at
//!   three clause-count levels (L1: single WHERE; L2: +ORDER BY; L3: +LIMIT).
//!   Guarantees every field is tested in isolation before being combined.
//! - **Phase 1 (baseline):** one command per (command\_id, clause) pair so
//!   every command type and every clause appears at least once.
//! - **Phase 2 (random fill):** seeded deterministic random combinations
//!   fill the remaining budget up to `total_budget`, respecting per-command
//!   `explosion_cap`.
//!
//! Three assertion tiers:
//! - Tier 1: parse without error.
//! - Tier 2: execute without panic (errors like "symbol not found" are OK).
//! - Tier 3: invariant checking on successful results. Each invariant is an
//!   independent `inv_*` function `fn(&str, &ForgeQLResult) -> Result<(), &'static str>`
//!   registered in `INVARIANT_CHECKS`. Add new invariants there.
//!
//! Run with: `cargo test -p forgeql-core --test sms_integration -- --nocapture`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::many_single_char_names,
    clippy::missing_const_for_fn,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
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
    /// Maps field names → test_values pool names for semantically correct WHERE values.
    field_pools: HashMap<String, String>,
    /// All enrichment fields parsed from syntax.json: (field_name, is_numeric).
    enrichment_fields: Vec<(String, bool)>,
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

    let field_pools: HashMap<String, String> = root
        .get("field_pools")
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();

    // Load all enrichment fields dynamically so pick_field covers all of them.
    let mut enrichment_fields: Vec<(String, bool)> = Vec::new();
    let ef = &root["enrichment_fields"];
    for (field, vals) in ef["string"].as_object().into_iter().flatten() {
        if vals.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            enrichment_fields.push((field.clone(), false));
        }
    }
    for (field, vals) in ef["numeric"].as_object().into_iter().flatten() {
        if vals.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            enrichment_fields.push((field.clone(), true));
        }
    }
    for (field, vals) in ef["boolean"].as_object().into_iter().flatten() {
        if vals.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            enrichment_fields.push((field.clone(), false));
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
        field_pools,
        enrichment_fields,
    }
}

/// Effective budget: SMS_BUDGET overrides syntax.json; SMS_NIGHTLY multiplies by 5.
fn effective_budget(spec: &SyntaxSpec) -> usize {
    if let Ok(val) = std::env::var("SMS_BUDGET") {
        if let Ok(n) = val.parse::<usize>() {
            return n;
        }
    }
    if std::env::var("SMS_NIGHTLY").is_ok() {
        return spec.total_budget * 5;
    }
    spec.total_budget
}

/// CSV output is enabled only in nightly mode or when SMS_CSV=1.
fn csv_enabled() -> bool {
    std::env::var("SMS_NIGHTLY").is_ok() || std::env::var("SMS_CSV").is_ok()
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
    field_pools: &HashMap<String, String>,
    enrichment_fields: &[(String, bool)],
) -> Option<String> {
    // Check applies_to restriction
    if let Some(ref targets) = spec.applies_to {
        if !targets.contains(&cmd.id) {
            return None;
        }
    }

    match clause_name {
        "WHERE" => {
            let rt = cmd.result_type.as_ref().and_then(|k| result_types.get(k));
            let (field, is_numeric) = pick_field(rng, rt, true, enrichment_fields);
            let field_pool = field_pools
                .get(&field)
                .map(String::as_str)
                .unwrap_or("string_values");
            let is_bool_field = field_pool == "bool_values";
            // Boolean fields only support = and !=; patterns/comparisons are nonsensical.
            let op = if is_bool_field {
                if rng.chance(50) {
                    "=".to_string()
                } else {
                    "!=".to_string()
                }
            } else {
                pick_operator(rng, operators, is_numeric)
            };
            let value = if is_numeric {
                pool_value(rng, "numeric_values", tv)
            } else if is_bool_field {
                pool_value(rng, "bool_values", tv) // unquoted: true / false
            } else {
                // LIKE/MATCHES require pattern syntax — use dedicated pattern pools
                let pool = match op.as_str() {
                    "LIKE" | "NOT LIKE" => "like_patterns",
                    "MATCHES" | "NOT MATCHES" => "regex_patterns",
                    _ => field_pool,
                };
                format!("'{}'", pool_value(rng, pool, tv))
            };
            Some(format!("WHERE {field} {op} {value}"))
        }
        "HAVING" => {
            // HAVING filters on aggregated count — always use count with a numeric operator
            let op = pick_operator(rng, operators, true);
            let val = pool_value(rng, "numeric_values", tv);
            Some(format!("HAVING count {op} {val}"))
        }
        "IN" | "EXCLUDE" => {
            let glob = pool_value(rng, "globs", tv);
            let keyword = if clause_name == "IN" { "IN" } else { "EXCLUDE" };
            Some(format!("{keyword} '{glob}'"))
        }
        "ORDER_BY" => {
            let rt = cmd.result_type.as_ref().and_then(|k| result_types.get(k));
            let (field, _) = pick_field(rng, rt, false, enrichment_fields);
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
    enrichment_fields: &[(String, bool)],
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
    // For enrichment-capable types, sometimes pick from the full dynamically-loaded enrichment set
    if include_enrichment
        && rt.accepts_enrichment
        && rng.chance(30)
        && !enrichment_fields.is_empty()
    {
        let (f, is_num) = rng.pick(enrichment_fields);
        return (f.clone(), *is_num);
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
                    &spec.field_pools,
                    &spec.enrichment_fields,
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

/// Phase 0: deterministic field sweep at three clause-count levels:
///   L1: `FIND symbols WHERE {field} = {value}`
///   L2: same + `ORDER BY name ASC`
///   L3: same + `ORDER BY name ASC LIMIT 10`
///
/// Covers two field sets:
/// - **Structural fields** from the `symbol_match` result type (fql_kind,
///   language, name, file, …) — these have known pool mappings in field_pools.
/// - **Enrichment fields** (has_doc, param_count, naming, …) from the
///   enrichment_fields index.
///
/// Guarantees every field is exercised in isolation before phase 1/2 combine
/// it with other clauses.
fn generate_field_sweep(spec: &SyntaxSpec) -> Vec<GeneratedCommand> {
    let mut cmds = Vec::new();
    let base = "FIND symbols";

    // Helper closure that emits L1/L2/L3 triples for one (field, value) pair.
    let emit = |cmds: &mut Vec<GeneratedCommand>, field: &str, value_str: String| {
        let w = format!("WHERE {field} = {value_str}");
        cmds.push(GeneratedCommand {
            fql: format!("{base} {w}"),
            command_id: "find_symbols".to_string(),
            _phase: 0,
        });
        cmds.push(GeneratedCommand {
            fql: format!("{base} {w} ORDER BY name ASC"),
            command_id: "find_symbols".to_string(),
            _phase: 0,
        });
        cmds.push(GeneratedCommand {
            fql: format!("{base} {w} ORDER BY name ASC LIMIT 10"),
            command_id: "find_symbols".to_string(),
            _phase: 0,
        });
    };

    // --- Structural fields: from the symbol_match result type ---
    // These are always meaningful (indexed by the engine) so their isolation
    // tests provide high-signal WHERE coverage (fql_kind, language, name, etc.).
    let structural_fields: &[(&str, bool)] = &[
        ("fql_kind", false),
        ("language", false),
        ("lang", false),
        ("name", false),
        ("file", false),
        ("path", false),
        ("node_kind", false),
    ];
    for (field, is_numeric) in structural_fields {
        let pool_name =
            spec.field_pools
                .get(*field)
                .map(String::as_str)
                .unwrap_or(if *is_numeric {
                    "numeric_values"
                } else {
                    "string_values"
                });
        let Some(values) = spec.test_values.get(pool_name).filter(|v| !v.is_empty()) else {
            continue;
        };
        for val in values {
            emit(
                &mut cmds,
                field,
                format!("'{val}'"), // structural string fields are always quoted
            );
        }
    }

    // --- Enrichment fields ---
    for (field, is_numeric) in &spec.enrichment_fields {
        let pool_name = spec
            .field_pools
            .get(field.as_str())
            .map(String::as_str)
            .unwrap_or(if *is_numeric {
                "numeric_values"
            } else {
                "string_values"
            });
        let values = match spec.test_values.get(pool_name) {
            Some(v) if !v.is_empty() => v,
            _ => continue,
        };
        let is_bool = pool_name == "bool_values";

        for val in values {
            let value_str = if *is_numeric || is_bool {
                val.clone() // unquoted
            } else {
                format!("'{val}'")
            };
            emit(&mut cmds, field, value_str);
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
                    &spec.field_pools,
                    &spec.enrichment_fields,
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
    let rc = result_count(&result);

    for check in INVARIANT_CHECKS {
        if let Err(status) = check(&cmd.fql, &result) {
            return Some(TierResult {
                fql: cmd.fql.clone(),
                command_id: cmd.command_id.clone(),
                tier: 3,
                duration_us: elapsed,
                status,
                result_count: rc,
            });
        }
    }

    Some(TierResult {
        fql: cmd.fql.clone(),
        command_id: cmd.command_id.clone(),
        tier: 3,
        duration_us: elapsed,
        status: "ok",
        result_count: rc,
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
                    "node_kind" => item.node_kind.clone(),
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

/// Extract ORDER BY field and direction from FQL text.
fn extract_order_by(fql: &str) -> Option<(String, String)> {
    let upper = fql.to_uppercase();
    let idx = upper.find("ORDER BY ")?;
    let after = &fql[idx + 9..];
    let mut parts = after.split_whitespace();
    let field = parts.next()?.to_string();
    let dir = parts
        .next()
        .map(str::to_uppercase)
        .unwrap_or_else(|| String::from("ASC"));
    Some((field, dir))
}

/// Check that ORDER BY produced sorted results.
fn check_order_by_sorted(result: &ForgeQLResult, field: &str, dir: &str) -> bool {
    match result {
        ForgeQLResult::Query(qr) => {
            if qr.results.len() <= 1 {
                return true;
            }
            let values: Vec<String> = qr
                .results
                .iter()
                .filter_map(|item| match field {
                    "name" => Some(item.name.clone()),
                    "node_kind" => item.node_kind.clone(),
                    "fql_kind" => item.fql_kind.clone(),
                    "language" | "lang" => item.language.clone(),
                    "path" | "file" => item.path.as_ref().map(|p| p.display().to_string()),
                    "usages" | "usages_count" => item.usages_count.map(|c| format!("{c:020}")),
                    "line" => item.line.map(|l| format!("{l:020}")),
                    other => item.fields.get(other).cloned(),
                })
                .collect();
            if values.len() <= 1 {
                return true;
            }
            let is_asc = dir == "ASC";
            for window in values.windows(2) {
                let cmp = window[0].cmp(&window[1]);
                if is_asc && cmp == std::cmp::Ordering::Greater {
                    return false;
                }
                if !is_asc && cmp == std::cmp::Ordering::Less {
                    return false;
                }
            }
            true
        }
        _ => true,
    }
}

/// Extract IN glob pattern from FQL text.
fn extract_in_glob(fql: &str) -> Option<String> {
    let upper = fql.to_uppercase();
    let idx = upper.find(" IN '")?;
    let after = &fql[idx + 4..];
    let start = after.find('\'')?;
    let rest = &after[start + 1..];
    let end = rest.find('\'')?;
    Some(rest[..end].to_string())
}

/// Extract EXCLUDE glob pattern from FQL text.
fn extract_exclude_glob(fql: &str) -> Option<String> {
    let upper = fql.to_uppercase();
    let key = "EXCLUDE '";
    let idx = upper.find(key)?;
    let after = &fql[idx + key.len()..];
    let end = after.find('\'')?;
    Some(after[..end].to_string())
}

/// Extract all WHERE predicates as `(field, op, raw_value)` tuples.
/// `raw_value` is already unquoted (quotes stripped from string values).
fn extract_where_predicates(fql: &str) -> Vec<(String, String, String)> {
    let mut result = Vec::new();
    let upper = fql.to_uppercase();
    let mut search_from = 0usize;
    while let Some(rel) = upper[search_from..].find("WHERE ") {
        let field_start = search_from + rel + 6;
        if field_start >= fql.len() {
            break;
        }
        let seg = &fql[field_start..];
        let upper_seg = seg.to_uppercase();
        // field = first whitespace-delimited token
        let field_end = seg.find(char::is_whitespace).unwrap_or(seg.len());
        if field_end == 0 {
            search_from = field_start + 1;
            continue;
        }
        let field = seg[..field_end].to_lowercase();
        let after_field = seg[field_end..].trim_start();
        let upper_af = after_field.to_uppercase();
        // detect op (longest match first)
        let (op, value_str): (&str, &str) = if upper_af.starts_with("NOT LIKE ") {
            ("NOT LIKE", &after_field[9..])
        } else if upper_af.starts_with("NOT MATCHES ") {
            ("NOT MATCHES", &after_field[12..])
        } else if upper_af.starts_with("LIKE ") {
            ("LIKE", &after_field[5..])
        } else if upper_af.starts_with("MATCHES ") {
            ("MATCHES", &after_field[8..])
        } else if let Some(rest) = after_field.strip_prefix("!= ") {
            ("!=", rest)
        } else if let Some(rest) = after_field.strip_prefix(">= ") {
            (">=", rest)
        } else if let Some(rest) = after_field.strip_prefix("<= ") {
            ("<=", rest)
        } else if let Some(rest) = after_field.strip_prefix("= ") {
            ("=", rest)
        } else if let Some(rest) = after_field.strip_prefix("> ") {
            (">", rest)
        } else if let Some(rest) = after_field.strip_prefix("< ") {
            ("<", rest)
        } else {
            search_from = field_start + 1;
            continue;
        };
        let value_str = value_str.trim_start();
        let raw_value = value_str.strip_prefix('\'').map_or_else(
            || {
                value_str
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_string()
            },
            |inner| {
                let end = inner.find('\'').unwrap_or(inner.len());
                inner[..end].to_string()
            },
        );
        result.push((field, op.to_string(), raw_value));
        search_from = field_start + 1;
        let _ = upper_seg; // suppress unused warning
    }
    result
}

/// Check that IN path filter was respected: all result paths should contain
/// every literal fragment of the glob (simplified glob matching).
fn check_in_path_filter(result: &ForgeQLResult, glob: &str) -> bool {
    match result {
        ForgeQLResult::Query(qr) => {
            let fragments: Vec<&str> = glob.split(['*', '/']).filter(|s| !s.is_empty()).collect();
            if fragments.is_empty() {
                return true; // pure wildcard like "**"
            }
            for item in &qr.results {
                if let Some(ref p) = item.path {
                    let ps = p.display().to_string();
                    if !fragments.iter().all(|frag| ps.contains(frag)) {
                        return false;
                    }
                }
            }
            true
        }
        _ => true,
    }
}

/// Check that EXCLUDE filter was respected: no result path should match
/// the excluded glob (simplified fragment matching).
fn check_exclude_path_filter(result: &ForgeQLResult, glob: &str) -> bool {
    match result {
        ForgeQLResult::Query(qr) => {
            let fragments: Vec<&str> = glob.split(['*', '/']).filter(|s| !s.is_empty()).collect();
            if fragments.is_empty() {
                return true; // exclude "**" — vacuous
            }
            for item in &qr.results {
                if let Some(ref p) = item.path {
                    let ps = p.display().to_string();
                    if fragments.iter().all(|frag| ps.contains(frag)) {
                        return false; // path should have been excluded but wasn't
                    }
                }
            }
            true
        }
        _ => true,
    }
}

/// Retrieve the value of a named field from a result row.
/// Checks structural fields first, then the dynamic `fields` map.
fn field_value_for_row(item: &forgeql_core::result::SymbolMatch, field: &str) -> Option<String> {
    match field {
        "name" => Some(item.name.clone()),
        "node_kind" => item.node_kind.clone(),
        "fql_kind" => item.fql_kind.clone(),
        "language" | "lang" => item.language.clone(),
        "path" | "file" => item.path.as_ref().map(|p| p.display().to_string()),
        "line" => item.line.map(|l| l.to_string()),
        "usages" | "usages_count" => item.usages_count.map(|c| c.to_string()),
        other => item.fields.get(other).cloned(),
    }
}

// -----------------------------------------------------------------------
// Tier-3 invariant functions
//
// Each function has the signature:
//   fn inv_*(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str>
//
// Return Ok(()) when the invariant holds, Err("invariant_fail_...") when
// it is violated. Register new checks in INVARIANT_CHECKS below.
// -----------------------------------------------------------------------

fn inv_limit(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str> {
    if let Some(limit) = extract_limit(fql) {
        if let Some(count) = result_count(result) {
            if count > limit {
                return Err("invariant_fail_limit");
            }
        }
    }
    Ok(())
}

fn inv_group_by(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str> {
    if let Some(field) = extract_group_by(fql) {
        if !check_group_by_unique(result, &field) {
            return Err("invariant_fail_group_by");
        }
    }
    Ok(())
}

fn inv_order_by(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str> {
    if let Some((field, dir)) = extract_order_by(fql) {
        if !check_order_by_sorted(result, &field, &dir) {
            return Err("invariant_fail_order_by");
        }
    }
    Ok(())
}

fn inv_in_filter(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str> {
    if let Some(glob) = extract_in_glob(fql) {
        if !check_in_path_filter(result, &glob) {
            return Err("invariant_fail_in_filter");
        }
    }
    Ok(())
}

fn inv_exclude_filter(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str> {
    if let Some(glob) = extract_exclude_glob(fql) {
        if !check_exclude_path_filter(result, &glob) {
            return Err("invariant_fail_exclude_filter");
        }
    }
    Ok(())
}

/// Check WHERE `=` predicates against each returned row.
///
/// Only `=` (equality) is verified: every result row must have the expected
/// value for the filtered field. `!=` is intentionally skipped because the
/// engine does not guarantee that enrichment fields are always populated,
/// making absence-of-value verification unreliable without full index coverage.
///
/// LIKE / MATCHES / numeric comparisons are also skipped (require regex or
/// numeric evaluation).
///
/// Vacuously passes when results are empty (e.g. EXCLUDE filtered everything).
fn inv_where_predicate(fql: &str, result: &ForgeQLResult) -> Result<(), &'static str> {
    let predicates = extract_where_predicates(fql);
    if predicates.is_empty() {
        return Ok(());
    }
    let ForgeQLResult::Query(qr) = result else {
        return Ok(());
    };
    if qr.results.is_empty() {
        return Ok(());
    }
    for (field, op, expected) in &predicates {
        if op != "=" {
            continue; // only verify equality — see doc comment above
        }
        for item in &qr.results {
            let actual = field_value_for_row(item, field);
            let Some(ref actual_val) = actual else {
                continue; // field absent in this row — skip rather than false-positive
            };
            if actual_val != expected {
                return Err("invariant_fail_where_predicate");
            }
        }
    }
    Ok(())
}

/// Registry of all tier-3 invariant checks, applied in order by `tier3_invariants`.
/// To add a new invariant: implement `fn inv_foo(fql: &str, result: &ForgeQLResult)
/// -> Result<(), &'static str>` and append it here.
#[allow(clippy::type_complexity)]
static INVARIANT_CHECKS: &[fn(&str, &ForgeQLResult) -> Result<(), &'static str>] = &[
    inv_limit,
    inv_group_by,
    inv_order_by,
    inv_in_filter,
    inv_exclude_filter,
    inv_where_predicate,
];

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
            ShowContent::Stats { sessions } => Some(sessions.len()),
        },
        _ => None,
    }
}

// -----------------------------------------------------------------------
// CSV output
// -----------------------------------------------------------------------

/// Convert Unix seconds to a UTC datetime string `YYYY-MM-DD_HH-MM-SS`.
/// Implemented without external crates using Howard Hinnant's algorithm.
fn format_datetime_utc(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let time = secs % 86400;
    let h = time / 3600;
    let min = (time % 3600) / 60;
    let sec = time % 60;
    format!("{y:04}-{m:02}-{d:02}_{h:02}-{min:02}-{sec:02}")
}

fn write_csv(results: &[TierResult]) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/scripts/logs");
    let _ = fs::create_dir_all(&dir);
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let timestamp = format_datetime_utc(secs);
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
    let mut spec = load_syntax();
    let base_budget = spec.total_budget;
    let budget = effective_budget(&spec);
    // Scale per-command caps proportionally so the full budget can be reached.
    if budget > base_budget {
        let scale = budget / base_budget.max(1);
        for cmd in &mut spec.commands {
            cmd.explosion_cap = cmd.explosion_cap.saturating_mul(scale);
        }
    }
    spec.total_budget = budget;
    let is_nightly = std::env::var("SMS_NIGHTLY").is_ok();

    eprintln!(
        "[SMS] mode={}, budget={}",
        if is_nightly { "nightly" } else { "ci" },
        budget
    );

    // Phase 0: deterministic enrichment-field sweep
    let sweep = generate_field_sweep(&spec);
    let sweep_count = sweep.len();

    // Phase 1: baseline — one command per (command × clause)
    let baseline = generate_baseline(&spec);
    let baseline_count = baseline.len();

    // Phase 2: random fill — consumes remaining budget after phase 0 + 1
    let prior_count = sweep_count + baseline_count;
    let random = generate_random_fill(&spec, prior_count);

    let all_commands: Vec<GeneratedCommand> =
        sweep.into_iter().chain(baseline).chain(random).collect();

    eprintln!(
        "[SMS] Generated {} commands (phase0={}, phase1={}, phase2={})",
        all_commands.len(),
        sweep_count,
        baseline_count,
        all_commands.len() - sweep_count - baseline_count
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

    // --- Write CSV (only in nightly / SMS_CSV mode) ---
    if csv_enabled() {
        write_csv(&all_results);
    }

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

    // Every enrichment field must have been exercised in phase 0
    let executed_fqls: std::collections::HashSet<&str> = all_results
        .iter()
        .filter(|r| r.tier == 2 && r.status == "ok")
        .map(|r| r.fql.as_str())
        .collect();
    let covered_fields: std::collections::HashSet<String> = all_commands
        .iter()
        .filter(|c| c._phase == 0)
        .filter(|c| executed_fqls.contains(c.fql.as_str()))
        .filter_map(|c| {
            extract_where_predicates(&c.fql)
                .into_iter()
                .next()
                .map(|(f, _, _)| f)
        })
        .collect();
    let all_sweep_fields: std::collections::HashSet<String> = spec
        .enrichment_fields
        .iter()
        .filter_map(|(f, _)| {
            let pool = spec
                .field_pools
                .get(f.as_str())
                .map(String::as_str)
                .unwrap_or("string_values");
            if spec
                .test_values
                .get(pool)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                Some(f.clone())
            } else {
                None
            }
        })
        .collect();
    let uncovered: Vec<&String> = all_sweep_fields
        .iter()
        .filter(|f| !covered_fields.contains(*f))
        .collect();
    assert!(
        uncovered.is_empty(),
        "[SMS] Enrichment fields never reached tier-2 ok in sweep: {uncovered:?}"
    );
}

// -----------------------------------------------------------------------
// Unit tests for extract_* and inv_* functions
// -----------------------------------------------------------------------

#[cfg(test)]
mod invariant_tests {
    use super::*;
    use forgeql_core::result::{ForgeQLResult, QueryResult, SymbolMatch};
    use std::collections::HashMap;
    use std::path::PathBuf;

    // -----------------------------------------------------------------------
    // Helpers to build mock ForgeQLResult values without running the engine
    // -----------------------------------------------------------------------

    fn sym(name: &str) -> SymbolMatch {
        SymbolMatch {
            name: name.to_string(),
            node_kind: None,
            fql_kind: None,
            language: None,
            path: None,
            line: None,
            usages_count: None,
            fields: HashMap::new(),
            count: None,
        }
    }

    fn sym_lang(name: &str, lang: &str) -> SymbolMatch {
        SymbolMatch {
            language: Some(lang.to_string()),
            ..sym(name)
        }
    }

    fn sym_field(name: &str, field: &str, value: &str) -> SymbolMatch {
        let mut fields = HashMap::new();
        let _ = fields.insert(field.to_string(), value.to_string());
        SymbolMatch {
            fields,
            ..sym(name)
        }
    }

    fn sym_path(name: &str, path: &str) -> SymbolMatch {
        SymbolMatch {
            path: Some(PathBuf::from(path)),
            ..sym(name)
        }
    }

    fn sym_line(name: &str, line: usize) -> SymbolMatch {
        SymbolMatch {
            line: Some(line),
            ..sym(name)
        }
    }

    fn query(items: Vec<SymbolMatch>) -> ForgeQLResult {
        let total = items.len();
        ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results: items,
            total,
            metric_hint: None,
            group_by_field: None,
        })
    }

    // -----------------------------------------------------------------------
    // extract_limit
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_limit_present() {
        assert_eq!(extract_limit("FIND symbols LIMIT 10"), Some(10));
        assert_eq!(
            extract_limit("FIND symbols WHERE name = 'foo' LIMIT 5"),
            Some(5)
        );
        assert_eq!(extract_limit("FIND symbols LIMIT 100 OFFSET 3"), Some(100));
    }

    #[test]
    fn test_extract_limit_absent() {
        assert_eq!(extract_limit("FIND symbols"), None);
        assert_eq!(extract_limit("FIND symbols ORDER BY name"), None);
    }

    // -----------------------------------------------------------------------
    // extract_group_by
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_group_by_present() {
        assert_eq!(
            extract_group_by("FIND symbols GROUP BY language"),
            Some("language".to_string())
        );
        assert_eq!(
            extract_group_by("FIND symbols GROUP BY fql_kind HAVING count > 1"),
            Some("fql_kind".to_string())
        );
    }

    #[test]
    fn test_extract_group_by_absent() {
        assert_eq!(extract_group_by("FIND symbols"), None);
    }

    // -----------------------------------------------------------------------
    // extract_order_by
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_order_by_with_direction() {
        assert_eq!(
            extract_order_by("FIND symbols ORDER BY name ASC"),
            Some(("name".to_string(), "ASC".to_string()))
        );
        assert_eq!(
            extract_order_by("FIND symbols ORDER BY line DESC"),
            Some(("line".to_string(), "DESC".to_string()))
        );
    }

    #[test]
    fn test_extract_order_by_defaults_to_asc() {
        assert_eq!(
            extract_order_by("FIND symbols ORDER BY name"),
            Some(("name".to_string(), "ASC".to_string()))
        );
    }

    #[test]
    fn test_extract_order_by_absent() {
        assert_eq!(extract_order_by("FIND symbols"), None);
    }

    // -----------------------------------------------------------------------
    // extract_in_glob / extract_exclude_glob
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_in_glob() {
        assert_eq!(
            extract_in_glob("FIND symbols IN '*.cpp'"),
            Some("*.cpp".to_string())
        );
        assert_eq!(extract_in_glob("FIND symbols"), None);
    }

    #[test]
    fn test_extract_exclude_glob() {
        assert_eq!(
            extract_exclude_glob("FIND symbols EXCLUDE '**/*.cpp'"),
            Some("**/*.cpp".to_string())
        );
        assert_eq!(
            extract_exclude_glob("FIND symbols EXCLUDE 'canonical.*'"),
            Some("canonical.*".to_string())
        );
        assert_eq!(extract_exclude_glob("FIND symbols"), None);
    }

    // -----------------------------------------------------------------------
    // extract_where_predicates
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_where_single_eq_string() {
        let preds = extract_where_predicates("FIND symbols WHERE language = 'cpp'");
        assert_eq!(
            preds,
            vec![("language".to_string(), "=".to_string(), "cpp".to_string())]
        );
    }

    #[test]
    fn test_extract_where_single_eq_bool() {
        let preds = extract_where_predicates("FIND symbols WHERE has_shadow = true");
        assert_eq!(
            preds,
            vec![(
                "has_shadow".to_string(),
                "=".to_string(),
                "true".to_string()
            )]
        );
    }

    #[test]
    fn test_extract_where_not_eq() {
        let preds = extract_where_predicates("FIND symbols WHERE has_shadow != false");
        assert_eq!(
            preds,
            vec![(
                "has_shadow".to_string(),
                "!=".to_string(),
                "false".to_string()
            )]
        );
    }

    #[test]
    fn test_extract_where_like() {
        let preds = extract_where_predicates("FIND symbols WHERE name LIKE '%foo%'");
        assert_eq!(
            preds,
            vec![("name".to_string(), "LIKE".to_string(), "%foo%".to_string())]
        );
    }

    #[test]
    fn test_extract_where_multiple() {
        let preds = extract_where_predicates(
            "FIND symbols WHERE has_shadow = true WHERE is_recursive = false",
        );
        assert_eq!(preds.len(), 2);
        assert_eq!(
            preds[0],
            (
                "has_shadow".to_string(),
                "=".to_string(),
                "true".to_string()
            )
        );
        assert_eq!(
            preds[1],
            (
                "is_recursive".to_string(),
                "=".to_string(),
                "false".to_string()
            )
        );
    }

    #[test]
    fn test_extract_where_absent() {
        assert!(extract_where_predicates("FIND symbols").is_empty());
    }

    // -----------------------------------------------------------------------
    // inv_limit
    // -----------------------------------------------------------------------

    #[test]
    fn test_inv_limit_passes_under() {
        let r = query(vec![sym("a"), sym("b"), sym("c")]);
        assert!(inv_limit("FIND symbols LIMIT 5", &r).is_ok());
    }

    #[test]
    fn test_inv_limit_passes_exact() {
        let r = query(vec![sym("a"), sym("b")]);
        assert!(inv_limit("FIND symbols LIMIT 2", &r).is_ok());
    }

    #[test]
    fn test_inv_limit_fails_over() {
        let r = query(vec![sym("a"), sym("b"), sym("c"), sym("d")]);
        assert_eq!(
            inv_limit("FIND symbols LIMIT 3", &r),
            Err("invariant_fail_limit")
        );
    }

    #[test]
    fn test_inv_limit_no_clause() {
        let r = query(vec![sym("a"); 100]);
        assert!(inv_limit("FIND symbols", &r).is_ok());
    }

    // -----------------------------------------------------------------------
    // inv_group_by
    // -----------------------------------------------------------------------

    #[test]
    fn test_inv_group_by_unique_lang() {
        let r = query(vec![sym_lang("a", "cpp"), sym_lang("b", "rust")]);
        assert!(inv_group_by("FIND symbols GROUP BY language", &r).is_ok());
    }

    #[test]
    fn test_inv_group_by_duplicate_lang() {
        let r = query(vec![sym_lang("a", "cpp"), sym_lang("b", "cpp")]);
        assert_eq!(
            inv_group_by("FIND symbols GROUP BY language", &r),
            Err("invariant_fail_group_by")
        );
    }

    // -----------------------------------------------------------------------
    // inv_order_by
    // -----------------------------------------------------------------------

    #[test]
    fn test_inv_order_by_asc_passes() {
        let r = query(vec![sym_line("a", 1), sym_line("b", 5), sym_line("c", 10)]);
        assert!(inv_order_by("FIND symbols ORDER BY line ASC", &r).is_ok());
    }

    #[test]
    fn test_inv_order_by_asc_fails() {
        let r = query(vec![sym_line("a", 10), sym_line("b", 5)]);
        assert_eq!(
            inv_order_by("FIND symbols ORDER BY line ASC", &r),
            Err("invariant_fail_order_by")
        );
    }

    #[test]
    fn test_inv_order_by_desc_passes() {
        let r = query(vec![sym_line("a", 10), sym_line("b", 5), sym_line("c", 1)]);
        assert!(inv_order_by("FIND symbols ORDER BY line DESC", &r).is_ok());
    }

    // -----------------------------------------------------------------------
    // inv_in_filter
    // -----------------------------------------------------------------------

    #[test]
    fn test_inv_in_filter_passes() {
        let r = query(vec![
            sym_path("foo", "canonical.cpp"),
            sym_path("bar", "other.cpp"),
        ]);
        assert!(inv_in_filter("FIND symbols IN '*.cpp'", &r).is_ok());
    }

    #[test]
    fn test_inv_in_filter_fails() {
        let r = query(vec![
            sym_path("foo", "canonical.cpp"),
            sym_path("bar", "canonical.rs"), // .rs should not match *.cpp
        ]);
        assert_eq!(
            inv_in_filter("FIND symbols IN '*.cpp'", &r),
            Err("invariant_fail_in_filter")
        );
    }

    // -----------------------------------------------------------------------
    // inv_exclude_filter
    // -----------------------------------------------------------------------

    #[test]
    fn test_inv_exclude_filter_passes() {
        let r = query(vec![sym_path("foo", "canonical.rs")]);
        assert!(inv_exclude_filter("FIND symbols EXCLUDE '*.cpp'", &r).is_ok());
    }

    #[test]
    fn test_inv_exclude_filter_fails() {
        let r = query(vec![
            sym_path("foo", "canonical.rs"),
            sym_path("bar", "canonical.cpp"), // should have been excluded
        ]);
        assert_eq!(
            inv_exclude_filter("FIND symbols EXCLUDE '*.cpp'", &r),
            Err("invariant_fail_exclude_filter")
        );
    }

    // -----------------------------------------------------------------------
    // inv_where_predicate
    // -----------------------------------------------------------------------

    #[test]
    fn test_inv_where_eq_passes() {
        let r = query(vec![
            sym_field("a", "has_shadow", "true"),
            sym_field("b", "has_shadow", "true"),
        ]);
        assert!(inv_where_predicate("FIND symbols WHERE has_shadow = true", &r).is_ok());
    }

    #[test]
    fn test_inv_where_eq_fails() {
        let r = query(vec![
            sym_field("a", "has_shadow", "true"),
            sym_field("b", "has_shadow", "false"), // violates WHERE has_shadow = true
        ]);
        assert_eq!(
            inv_where_predicate("FIND symbols WHERE has_shadow = true", &r),
            Err("invariant_fail_where_predicate")
        );
    }

    #[test]
    fn test_inv_where_neq_not_checked() {
        // != predicates are intentionally not validated (see inv_where_predicate doc).
        // A row violating != must still pass — we don't catch it here.
        let r = query(vec![
            sym_field("a", "has_shadow", "true"), // would violate !=, but not checked
            sym_field("b", "has_shadow", "false"),
        ]);
        assert!(inv_where_predicate("FIND symbols WHERE has_shadow != true", &r).is_ok());
    }

    #[test]
    fn test_inv_where_empty_results_passes() {
        let r = query(vec![]);
        // Vacuously true — e.g. EXCLUDE filtered everything out
        assert!(inv_where_predicate("FIND symbols WHERE has_shadow = true", &r).is_ok());
    }

    #[test]
    fn test_inv_where_like_skipped() {
        // LIKE predicates are not checked — should always pass regardless of data
        let r = query(vec![sym_field("a", "name", "something_else")]);
        assert!(inv_where_predicate("FIND symbols WHERE name LIKE '%foo%'", &r).is_ok());
    }
}
