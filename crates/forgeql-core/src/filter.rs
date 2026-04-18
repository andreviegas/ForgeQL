/// Universal clause pipeline for `ForgeQL` read-only operations.
///
/// Every list-returning query pipes its raw results through [`apply_clauses`],
/// which applies path inclusion/exclusion, WHERE predicates, GROUP BY,
/// HAVING predicates, ORDER BY, OFFSET, and LIMIT — in that fixed order.
use std::cmp::Ordering;
use std::path::Path;

use crate::ir::{Clauses, CompareOp, GroupBy, PredicateValue, SortDirection};
use regex::Regex;

mod impls;

// -----------------------------------------------------------------------
// ClauseTarget trait
// -----------------------------------------------------------------------
/// Trait for result types that can be filtered by the generic clause pipeline.
///
/// Implementing types expose their fields through typed accessors:
/// - [`field_str`](ClauseTarget::field_str) — string / LIKE comparisons
/// - [`field_num`](ClauseTarget::field_num) — numeric comparisons
/// - [`path`](ClauseTarget::path) — glob include / exclude
pub trait ClauseTarget {
    /// Return the string value of a named field, or `None` if unknown.
    fn field_str(&self, field: &str) -> Option<&str>;

    /// Return the numeric value of a named field, or `None` if unknown.
    fn field_num(&self, field: &str) -> Option<i64>;

    /// File path of the item (for glob include / exclude).
    fn path(&self) -> Option<&Path>;

    /// Store the per-group aggregation count produced by GROUP BY.
    /// Default implementation is a no-op for types that don't support counts.
    fn set_count(&mut self, _count: usize) {}
}

// -----------------------------------------------------------------------
// Glob matching
// -----------------------------------------------------------------------

/// SQL-style `LIKE` pattern matching where `%` matches zero or more
/// characters and `_` matches exactly one.
///
/// The match is case-insensitive when both sides are ASCII.
#[must_use]
#[allow(clippy::indexing_slicing)] // DP algorithm — loop ranges guarantee bounds
pub fn like_match(text: &str, pattern: &str) -> bool {
    let text_chars: Vec<char> = text.to_ascii_lowercase().chars().collect();
    let pat_chars: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let (text_len, pat_len) = (text_chars.len(), pat_chars.len());

    let mut dp = vec![vec![false; pat_len + 1]; text_len + 1];
    dp[0][0] = true;

    for j in 1..=pat_len {
        if pat_chars[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        }
    }

    for i in 1..=text_len {
        for j in 1..=pat_len {
            dp[i][j] = match pat_chars[j - 1] {
                '%' => dp[i - 1][j] || dp[i][j - 1],
                '_' => dp[i - 1][j - 1],
                ch => ch == text_chars[i - 1] && dp[i - 1][j - 1],
            };
        }
    }

    dp[text_len][pat_len]
}

/// Check whether a path matches a glob pattern.
fn path_glob_matches(path: &Path, pattern: &str) -> bool {
    crate::ast::query::glob_matches(path, pattern)
}

// -----------------------------------------------------------------------
// Predicate evaluation
// -----------------------------------------------------------------------

/// Evaluate a single predicate against a `ClauseTarget` item.
pub fn eval_predicate<T: ClauseTarget>(item: &T, predicate: &crate::ir::Predicate) -> bool {
    match predicate.op {
        // ---- String / LIKE operators ----
        CompareOp::Like => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return false,
            };
            item.field_str(&predicate.field)
                .is_some_and(|v| like_match(v, pat))
        }
        CompareOp::NotLike => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return true,
            };
            item.field_str(&predicate.field)
                .is_some_and(|v| !like_match(v, pat))
        }
        // ---- Regex MATCHES operators ----
        CompareOp::Matches => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return false,
            };
            let Ok(re) = Regex::new(pat) else {
                return false;
            };
            item.field_str(&predicate.field)
                .is_some_and(|v| re.is_match(v))
        }
        CompareOp::NotMatches => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return true,
            };
            let Ok(re) = Regex::new(pat) else {
                return true;
            };
            item.field_str(&predicate.field)
                .is_some_and(|v| !re.is_match(v))
        }
        CompareOp::Eq => match &predicate.value {
            PredicateValue::String(s) => item
                .field_str(&predicate.field)
                .is_some_and(|v| v.eq_ignore_ascii_case(s)),
            PredicateValue::Number(n) => item.field_num(&predicate.field).is_some_and(|v| v == *n),
            PredicateValue::Bool(_) => false,
        },
        CompareOp::NotEq => match &predicate.value {
            PredicateValue::String(s) => item
                .field_str(&predicate.field)
                .is_some_and(|v| !v.eq_ignore_ascii_case(s)),
            PredicateValue::Number(n) => item.field_num(&predicate.field).is_some_and(|v| v != *n),
            PredicateValue::Bool(_) => false,
        },
        // ---- Numeric operators ----
        CompareOp::Gt => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(&predicate.field).is_some_and(|v| v > rhs)),
        CompareOp::Gte => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(&predicate.field).is_some_and(|v| v >= rhs)),
        CompareOp::Lt => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(&predicate.field).is_some_and(|v| v < rhs)),
        CompareOp::Lte => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(&predicate.field).is_some_and(|v| v <= rhs)),
    }
}

/// Extract numeric RHS, returning `None` for non-numeric values.
const fn numeric_rhs(value: &PredicateValue) -> Option<i64> {
    match value {
        PredicateValue::Number(n) => Some(*n),
        _ => None,
    }
}

// -----------------------------------------------------------------------
// Apply clauses — universal pipeline
// -----------------------------------------------------------------------

/// Apply the full clause pipeline to a mutable result set.
///
/// Steps in fixed order:
/// 1. `IN 'glob'`        — path glob inclusion
/// 2. `EXCLUDE 'glob'`   — path glob exclusion
/// 3. `WHERE …`          — predicate filtering (AND semantics)
/// 4. `GROUP BY <field>`  — deduplicate; keep first row per group value
/// 5. `HAVING …`         — predicate filtering on grouped results
/// 6. `ORDER BY <field>` — sort
/// 7. `OFFSET N`         — skip N items
/// 8. `LIMIT N`          — truncate to N items
pub fn apply_clauses<T: ClauseTarget>(results: &mut Vec<T>, clauses: &Clauses) {
    // 1. IN glob
    if let Some(ref glob) = clauses.in_glob {
        results.retain(|item| item.path().is_some_and(|p| path_glob_matches(p, glob)));
    }

    // 2. EXCLUDE glob
    if let Some(ref glob) = clauses.exclude_glob {
        results.retain(|item| item.path().is_none_or(|p| !path_glob_matches(p, glob)));
    }

    // 3. WHERE predicates
    for predicate in &clauses.where_predicates {
        let pred = predicate.clone();
        results.retain(|item| eval_predicate(item, &pred));
    }

    // 4. GROUP BY — deduplicate by group key and store per-group count in .count
    if let Some(GroupBy::Field(ref field)) = clauses.group_by {
        let field = field.clone();
        // Pass 1: count occurrences per group key.
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for item in results.iter() {
            let key = item.field_str(&field).map(String::from).unwrap_or_default();
            *counts.entry(key).or_insert(0) += 1;
        }
        // Pass 2: keep first row per group, write per-group count into it.
        let mut seen = std::collections::HashSet::new();
        let all = std::mem::take(results);
        for mut item in all {
            let key = item.field_str(&field).map(String::from).unwrap_or_default();
            if seen.insert(key.clone()) {
                if let Some(&n) = counts.get(&key) {
                    item.set_count(n);
                }
                results.push(item);
            }
        }
    }

    // 5. HAVING predicates
    for predicate in &clauses.having_predicates {
        let pred = predicate.clone();
        results.retain(|item| eval_predicate(item, &pred));
    }

    // 6. ORDER BY
    if let Some(ref order) = clauses.order_by {
        let field = order.field.clone();
        let direction = order.direction;
        results.sort_by(|a, b| {
            let primary = if let (Some(va), Some(vb)) = (a.field_num(&field), b.field_num(&field)) {
                match direction {
                    SortDirection::Desc => vb.cmp(&va),
                    SortDirection::Asc => va.cmp(&vb),
                }
            } else {
                let sa = a.field_str(&field).unwrap_or("");
                let sb = b.field_str(&field).unwrap_or("");
                match direction {
                    SortDirection::Asc => sa.cmp(sb),
                    SortDirection::Desc => sb.cmp(sa),
                }
            };
            if primary == Ordering::Equal {
                let na = a.field_str("name").unwrap_or("");
                let nb = b.field_str("name").unwrap_or("");
                na.cmp(nb)
            } else {
                primary
            }
        });
    }

    // 7. OFFSET
    let skip = clauses.offset.unwrap_or(0);
    if skip > 0 {
        let drained = skip.min(results.len());
        drop(results.drain(..drained));
    }

    // 8. LIMIT
    if let Some(max) = clauses.limit {
        results.truncate(max);
    }
}
// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Clauses, OrderBy, Predicate, PredicateValue};
    use crate::result::SymbolMatch;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn make_symbol(name: &str, kind: &str, usages: usize) -> SymbolMatch {
        SymbolMatch {
            name: name.to_string(),
            node_kind: None,
            fql_kind: Some(kind.to_string()),
            language: None,
            path: Some(PathBuf::from(format!("src/{name}.cpp"))),
            line: None,
            usages_count: Some(usages),
            fields: HashMap::new(),
            count: None,
        }
    }

    fn make_symbol_with_sig(name: &str, sig: &str, usages: usize) -> SymbolMatch {
        let mut sym = make_symbol(name, "Function", usages);
        sym.fields.insert("signature".to_string(), sig.to_string());
        sym
    }

    #[test]
    fn apply_clauses_filter_by_kind_eq() {
        let mut items = vec![
            make_symbol("foo", "Function", 3),
            make_symbol("bar", "Variable", 1),
            make_symbol("baz", "Function", 7),
        ];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "fql_kind".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("Function".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "foo");
        assert_eq!(items[1].name, "baz");
    }

    #[test]
    fn apply_clauses_numeric_predicate_gte() {
        let mut items = vec![
            make_symbol("a", "Function", 2),
            make_symbol("b", "Function", 5),
            make_symbol("c", "Function", 10),
        ];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "usages".into(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(5),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "b");
        assert_eq!(items[1].name, "c");
    }

    #[test]
    fn apply_clauses_order_by_desc_then_limit() {
        let mut items = vec![
            make_symbol("a", "Function", 2),
            make_symbol("c", "Function", 10),
            make_symbol("b", "Function", 5),
        ];
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "usages".into(),
                direction: SortDirection::Desc,
            }),
            limit: Some(2),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "c");
        assert_eq!(items[1].name, "b");
    }

    #[test]
    fn apply_clauses_order_by_asc() {
        let mut items = vec![
            make_symbol("c", "Function", 10),
            make_symbol("a", "Function", 2),
            make_symbol("b", "Function", 5),
        ];
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "usages".into(),
                direction: SortDirection::Asc,
            }),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].name, "b");
        assert_eq!(items[2].name, "c");
    }

    #[test]
    fn apply_clauses_name_like() {
        let mut items = vec![
            make_symbol("setPeakLevel", "Function", 3),
            make_symbol("getBaseLevel", "Function", 5),
            make_symbol("setMinIntensity", "Function", 1),
        ];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "name".into(),
                op: CompareOp::Like,
                value: PredicateValue::String("set%".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "setPeakLevel");
        assert_eq!(items[1].name, "setMinIntensity");
    }

    #[test]
    fn apply_clauses_signature_like_and_not_like() {
        let mut items = vec![
            make_symbol_with_sig("foo", "void foo(int x)", 1),
            make_symbol_with_sig("bar", "int bar(const char* s)", 2),
            make_symbol_with_sig("baz", "void baz()", 3),
        ];
        let clauses = Clauses {
            where_predicates: vec![
                Predicate {
                    field: "signature".into(),
                    op: CompareOp::Like,
                    value: PredicateValue::String("void%".into()),
                },
                Predicate {
                    field: "signature".into(),
                    op: CompareOp::NotLike,
                    value: PredicateValue::String("%int%".into()),
                },
            ],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "baz");
    }

    #[test]
    fn apply_clauses_exclude_glob() {
        let mut items = vec![
            SymbolMatch {
                name: "a".into(),
                path: Some(PathBuf::from("src/main.cpp")),
                node_kind: None,
                fql_kind: None,
                language: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: None,
            },
            SymbolMatch {
                name: "b".into(),
                path: Some(PathBuf::from("tests/test.cpp")),
                node_kind: None,
                fql_kind: None,
                language: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: None,
            },
        ];
        let clauses = Clauses {
            exclude_glob: Some("tests/**".into()),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "a");
    }

    #[test]
    fn apply_clauses_path_in_glob() {
        let mut items = vec![
            SymbolMatch {
                name: "a".into(),
                path: Some(PathBuf::from("src/main.cpp")),
                node_kind: None,
                fql_kind: None,
                language: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: None,
            },
            SymbolMatch {
                name: "b".into(),
                path: Some(PathBuf::from("include/header.hpp")),
                node_kind: None,
                fql_kind: None,
                language: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: None,
            },
        ];
        let clauses = Clauses {
            in_glob: Some("src/**".into()),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "a");
    }

    #[test]
    fn apply_clauses_combined_pipeline() {
        let mut items = vec![
            make_symbol("alpha", "Function", 1),
            make_symbol("beta", "Variable", 10),
            make_symbol("gamma", "Function", 8),
            make_symbol("delta", "Function", 3),
            make_symbol("epsilon", "Function", 12),
        ];
        let clauses = Clauses {
            where_predicates: vec![
                Predicate {
                    field: "fql_kind".into(),
                    op: CompareOp::Eq,
                    value: PredicateValue::String("Function".into()),
                },
                Predicate {
                    field: "usages".into(),
                    op: CompareOp::Gte,
                    value: PredicateValue::Number(3),
                },
            ],
            order_by: Some(OrderBy {
                field: "usages".into(),
                direction: SortDirection::Desc,
            }),
            limit: Some(2),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "epsilon"); // 12 usages
        assert_eq!(items[1].name, "gamma"); // 8 usages
    }

    #[test]
    fn like_match_basic_patterns() {
        assert!(like_match("setPeakLevel", "set%"));
        assert!(like_match("setPeakLevel", "%Peak%"));
        assert!(like_match("setPeakLevel", "%Level"));
        assert!(!like_match("setPeakLevel", "get%"));
        assert!(like_match("a", "_"));
        assert!(!like_match("ab", "_"));
        assert!(like_match("setPeakLevel", "%"));
    }

    #[test]
    fn like_match_case_insensitive() {
        assert!(like_match("SetPeakLevel", "set%"));
        assert!(like_match("setPeakLevel", "SET%"));
    }

    // -------------------------------------------------------------------
    // MATCHES (regex) predicate tests
    // -------------------------------------------------------------------

    #[test]
    fn matches_basic_regex() {
        let mut items = vec![
            make_symbol("setPeakLevel", "Function", 3),
            make_symbol("getBaseLevel", "Function", 5),
            make_symbol("init_motor", "Function", 1),
        ];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "name".into(),
                op: CompareOp::Matches,
                value: PredicateValue::String("^(set|get)".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "setPeakLevel");
        assert_eq!(items[1].name, "getBaseLevel");
    }

    #[test]
    fn not_matches_regex() {
        let mut items = vec![
            make_symbol("setPeakLevel", "Function", 3),
            make_symbol("getBaseLevel", "Function", 5),
            make_symbol("init_motor", "Function", 1),
        ];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "name".into(),
                op: CompareOp::NotMatches,
                value: PredicateValue::String("^(set|get)".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "init_motor");
    }

    #[test]
    fn matches_invalid_regex_returns_false() {
        let mut items = vec![make_symbol("foo", "Function", 1)];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "name".into(),
                op: CompareOp::Matches,
                value: PredicateValue::String("[invalid".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        // Invalid regex matches nothing — item is filtered out.
        assert_eq!(items.len(), 0);
    }

    // -------------------------------------------------------------------
    // SourceLine ClauseTarget tests
    // -------------------------------------------------------------------

    use crate::result::SourceLine;

    fn make_lines() -> Vec<SourceLine> {
        vec![
            SourceLine {
                line: 10,
                text: "void setup() {".into(),
                marker: None,
            },
            SourceLine {
                line: 11,
                text: "    // TODO: fix this".into(),
                marker: None,
            },
            SourceLine {
                line: 12,
                text: "    int x = 42;".into(),
                marker: None,
            },
            SourceLine {
                line: 13,
                text: "    // FIXME: needs review".into(),
                marker: None,
            },
            SourceLine {
                line: 14,
                text: "}".into(),
                marker: None,
            },
        ]
    }

    #[test]
    fn source_line_where_text_matches() {
        let mut lines = make_lines();
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "text".into(),
                op: CompareOp::Matches,
                value: PredicateValue::String("TODO|FIXME".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut lines, &clauses);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, 11);
        assert_eq!(lines[1].line, 13);
    }

    #[test]
    fn source_line_where_text_like() {
        let mut lines = make_lines();
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "text".into(),
                op: CompareOp::Like,
                value: PredicateValue::String("%int%".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut lines, &clauses);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].line, 12);
    }

    #[test]
    fn source_line_where_line_gte() {
        let mut lines = make_lines();
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "line".into(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(13),
            }],
            ..Default::default()
        };
        apply_clauses(&mut lines, &clauses);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].line, 13);
        assert_eq!(lines[1].line, 14);
    }

    // -------------------------------------------------------------------
    // CallGraphEntry ClauseTarget tests
    // -------------------------------------------------------------------

    use crate::result::CallGraphEntry;

    #[test]
    fn callgraph_where_name_eq_detects_recursion() {
        let mut entries = vec![
            CallGraphEntry {
                name: "helper".into(),
                path: Some(PathBuf::from("src/util.cpp")),
                line: Some(10),
                byte_start: None,
            },
            CallGraphEntry {
                name: "process".into(),
                path: Some(PathBuf::from("src/main.cpp")),
                line: Some(42),
                byte_start: None,
            },
            CallGraphEntry {
                name: "cleanup".into(),
                path: None,
                line: None,
                byte_start: None,
            },
        ];
        // Simulate: SHOW callees OF 'process' WHERE name = 'process'
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "name".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("process".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut entries, &clauses);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "process");
    }

    #[test]
    fn callgraph_where_name_matches() {
        let mut entries = vec![
            CallGraphEntry {
                name: "init_motor".into(),
                path: Some(PathBuf::from("src/motor.cpp")),
                line: Some(5),
                byte_start: None,
            },
            CallGraphEntry {
                name: "init_sensor".into(),
                path: Some(PathBuf::from("src/sensor.cpp")),
                line: Some(15),
                byte_start: None,
            },
            CallGraphEntry {
                name: "cleanup".into(),
                path: None,
                line: None,
                byte_start: None,
            },
        ];
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "name".into(),
                op: CompareOp::Matches,
                value: PredicateValue::String("^init_".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut entries, &clauses);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "init_motor");
        assert_eq!(entries[1].name, "init_sensor");
    }
}
