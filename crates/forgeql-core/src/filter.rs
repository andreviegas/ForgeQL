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

/// Extract literal substrings from a SQL `LIKE` pattern, suitable for
/// trigram-based candidate prefiltering.
///
/// `%` and `_` are wildcards and act as literal-run separators.  Any
/// returned string is a contiguous run of literal (non-wildcard) characters
/// that must appear verbatim in any matching value.
///
/// Example: `"%foo_bar%baz%"` \u2192 `["foo", "bar", "baz"]` (the `_` splits
/// the run because it represents a single arbitrary character).
#[must_use]
pub fn like_pattern_literals(pattern: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in pattern.chars() {
        if ch == '%' || ch == '_' {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(ch);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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
                .is_some_and(|v| v == s.as_str()),
            PredicateValue::Number(n) => item.field_num(&predicate.field).is_some_and(|v| v == *n),
            PredicateValue::Bool(_) => false,
        },
        CompareOp::NotEq => match &predicate.value {
            PredicateValue::String(s) => item
                .field_str(&predicate.field)
                .is_some_and(|v| v != s.as_str()),
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
// Top-K helpers (Phase 8)
// -----------------------------------------------------------------------

/// Maximum LIMIT value for which the bounded top-K path is activated.
/// Beyond this threshold the existing full-sort path is used.
pub(crate) const TOPK_THRESHOLD: usize = 1_000;

/// Compare two [`ClauseTarget`] items according to the ORDER BY clause in
/// `clauses`, including the deterministic `(name, line, path)` tie-breakers.
///
/// This is the single source-of-truth comparator shared by:
/// - the full sort in `apply_clauses` (step 6), and
/// - the bounded top-K path (`collect_top_k`), and
/// - the per-segment running heap in `ColumnarStorage::materialize_all`.
///
/// Returning [`Ordering::Less`] means `a` sorts *before* `b` (i.e. `a` is
/// the "better" row that should appear first in the output).
pub(crate) fn order_cmp<T: ClauseTarget>(a: &T, b: &T, clauses: &Clauses) -> Ordering {
    // Primary key — only when an explicit ORDER BY clause is present.
    if let Some(ref order_by) = clauses.order_by {
        let field = order_by.field.as_str();
        let primary = if let (Some(va), Some(vb)) = (a.field_num(field), b.field_num(field)) {
            match order_by.direction {
                SortDirection::Desc => vb.cmp(&va),
                SortDirection::Asc => va.cmp(&vb),
            }
        } else {
            let sa = a.field_str(field).unwrap_or("");
            let sb = b.field_str(field).unwrap_or("");
            match order_by.direction {
                SortDirection::Asc => sa.cmp(sb),
                SortDirection::Desc => sb.cmp(sa),
            }
        };
        if primary != Ordering::Equal {
            return primary;
        }
    }
    // Tie-breakers: name → line → path.  Guarantees a deterministic ordering
    // before LIMIT truncation so both storage backends return the same rows.
    let na = a.field_str("name").unwrap_or("");
    let nb = b.field_str("name").unwrap_or("");
    match na.cmp(nb) {
        Ordering::Equal => {}
        other => return other,
    }
    let la = a.field_num("line").unwrap_or(0);
    let lb = b.field_num("line").unwrap_or(0);
    match la.cmp(&lb) {
        Ordering::Equal => {}
        other => return other,
    }
    let pa = a.field_str("path").unwrap_or("");
    let pb = b.field_str("path").unwrap_or("");
    pa.cmp(pb)
}

/// Return the top-`k` items from `items` ranked by `cmp`, without fully
/// sorting the input.
///
/// Uses [`slice::select_nth_unstable_by`] (introselect, O(N) average) to
/// partition and then sorts only the k-element window (O(k log k)).
/// Falls back to a full sort when `items.len() <= k`.
///
/// # Comparator contract
/// `cmp(a, b) == Ordering::Less` means `a` is *better* (sorts earlier) than
/// `b`.  Same convention as [`order_cmp`].
pub(crate) fn collect_top_k<T, F>(mut items: Vec<T>, k: usize, cmp: F) -> Vec<T>
where
    F: Fn(&T, &T) -> Ordering,
{
    if k == 0 {
        return Vec::new();
    }
    if items.len() <= k {
        items.sort_by(|a, b| cmp(a, b));
        return items;
    }
    // Partition: items[..k] become the k "best" elements (unsorted),
    // items[k..] are all "worse".  O(N) average, O(N) worst case.
    let _ = items.select_nth_unstable_by(k - 1, |a, b| cmp(a, b));
    items.truncate(k);
    items.sort_by(|a, b| cmp(a, b));
    items
}

/// Extract the minimum length `N` from a bare `.{N,}` pattern (no anchors,
/// no max bound, no other content).  When matched, a simple `len >= N` check
/// is equivalent to the regex and avoids compiling and running the regex
/// engine entirely.
///
/// Examples: `".{150,}"` → `Some(150)`, `".{90,}"` → `Some(90)`.
/// Non-matching: `".{N,M}"`, `"^.{N,}$"`, `"foo.{N,}"` → `None`.
fn dot_brace_min_len(pattern: &str) -> Option<usize> {
    let inner = pattern.strip_prefix(".{")?.strip_suffix(",}")?;
    inner.parse::<usize>().ok()
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
    // MATCHES / NOT MATCHES are handled with two optimisations:
    //   a) ".{N,}" → simple `len >= N` check (no regex engine at all).
    //   b) All other patterns: compile the regex once per predicate, not
    //      once per item, avoiding millions of redundant compilations on
    //      large symbol tables (e.g. Linux kernel with 29 M+ symbols).
    for predicate in &clauses.where_predicates {
        if let (CompareOp::Matches | CompareOp::NotMatches, PredicateValue::String(pat)) =
            (&predicate.op, &predicate.value)
        {
            let is_matches = predicate.op == CompareOp::Matches;
            let field = predicate.field.clone();

            // Fast path: ".{N,}" ↔ len >= N  (no newlines assumed in the
            // target field, which holds for structural enrichment values
            // such as condition_text, signature, and name).
            if let Some(min_len) = dot_brace_min_len(pat) {
                results.retain(|item| {
                    let ok = item.field_str(&field).is_some_and(|v| v.len() >= min_len);
                    ok == is_matches
                });
                continue;
            }

            // General path: compile once, apply to all remaining items.
            match Regex::new(pat) {
                Ok(re) => {
                    results.retain(|item| {
                        let ok = item.field_str(&field).is_some_and(|v| re.is_match(v));
                        ok == is_matches
                    });
                }
                Err(_) => {
                    // Invalid regex: MATCHES → nothing passes; NOT MATCHES → all pass.
                    if is_matches {
                        results.clear();
                    }
                    // NOT MATCHES with invalid regex: retain all (no-op).
                }
            }
        } else {
            let pred = predicate.clone();
            results.retain(|item| eval_predicate(item, &pred));
        }
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

    // 6. ORDER BY (+ 7/8 fast-path for bounded top-K)
    //
    // A deterministic ordering is required *before* OFFSET/LIMIT so that
    // truncation picks the same rows across backends (legacy ↔ columnar).
    // Even when no ORDER BY is supplied we apply a stable default sort by
    // `(name, line, path)` to keep parity tests deterministic.
    //
    // Fast path: when ORDER BY is present, LIMIT <= TOPK_THRESHOLD, OFFSET
    // is zero, and GROUP BY is absent, use `collect_top_k` (introselect
    // O(N) avg) instead of a full O(N log N) sort.  Produces byte-identical
    // results via the shared `order_cmp` comparator.
    let want_topk = clauses.order_by.is_some()
        && clauses.group_by.is_none()
        && clauses.offset.unwrap_or(0) == 0
        && clauses.limit.is_some_and(|k| k <= TOPK_THRESHOLD);

    if let (Some(k), true) = (clauses.limit, want_topk) {
        let taken = std::mem::take(results);
        *results = collect_top_k(taken, k, |a, b| order_cmp(a, b, clauses));
        return; // OFFSET == 0 and LIMIT already applied by collect_top_k.
    }

    results.sort_by(|a, b| order_cmp(a, b, clauses));

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
        // Results are now deterministically ordered by (name, line, path)
        // even when no explicit ORDER BY is given — see apply_clauses.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "baz");
        assert_eq!(items[1].name, "foo");
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
        // Default deterministic order: name ASC.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "setMinIntensity");
        assert_eq!(items[1].name, "setPeakLevel");
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
        // Default deterministic order: name ASC.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "getBaseLevel");
        assert_eq!(items[1].name, "setPeakLevel");
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
                node_id: None,
            },
            SourceLine {
                line: 11,
                text: "    // TODO: fix this".into(),
                marker: None,
                node_id: None,
            },
            SourceLine {
                line: 12,
                text: "    int x = 42;".into(),
                marker: None,
                node_id: None,
            },
            SourceLine {
                line: 13,
                text: "    // FIXME: needs review".into(),
                marker: None,
                node_id: None,
            },
            SourceLine {
                line: 14,
                text: "}".into(),
                marker: None,
                node_id: None,
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

    // -- like_match edge cases -----------------------------------------

    #[test]
    fn like_match_empty_both() {
        assert!(like_match("", ""));
    }

    #[test]
    fn like_match_empty_pattern_nonempty_text() {
        assert!(!like_match("foo", ""));
    }

    #[test]
    fn like_match_percent_alone_matches_anything() {
        assert!(like_match("anything", "%"));
        assert!(like_match("", "%"));
    }

    #[test]
    fn like_match_underscore_at_start() {
        assert!(like_match("a", "_"));
        assert!(!like_match("", "_"));
    }

    #[test]
    fn like_match_underscore_at_end() {
        assert!(like_match("z", "_"));
        assert!(!like_match("ab", "_"));
    }

    #[test]
    fn like_match_consecutive_percent() {
        assert!(like_match("ab", "%%b"));
    }

    #[test]
    fn like_match_pattern_longer_than_text() {
        assert!(!like_match("ab", "abc"));
    }

    #[test]
    fn like_match_only_underscores() {
        assert!(like_match("ab", "__"));
        assert!(!like_match("a", "__"));
        assert!(!like_match("abc", "__"));
    }

    // -- path_glob_matches ---------------------------------------------

    #[test]
    fn path_glob_matches_exact_file() {
        assert!(path_glob_matches(
            std::path::Path::new("src/foo.rs"),
            "src/foo.rs"
        ));
    }

    #[test]
    fn path_glob_matches_no_match() {
        assert!(!path_glob_matches(
            std::path::Path::new("src/foo.h"),
            "src/**/*.cpp"
        ));
    }

    #[test]
    fn path_glob_matches_double_star() {
        assert!(path_glob_matches(
            std::path::Path::new("src/a/b/c.rs"),
            "src/**"
        ));
    }

    #[test]
    fn path_glob_matches_extension_wildcard() {
        assert!(path_glob_matches(
            std::path::Path::new("bar.cpp"),
            "**/*.cpp"
        ));
        assert!(!path_glob_matches(
            std::path::Path::new("bar.rs"),
            "**/*.cpp"
        ));
    }

    #[test]
    fn path_glob_matches_single_star() {
        assert!(path_glob_matches(
            std::path::Path::new("src/foo.rs"),
            "src/*.rs"
        ));
        // single * does not cross directory boundary
        assert!(!path_glob_matches(
            std::path::Path::new("src/sub/foo.rs"),
            "src/*.rs"
        ));
    }

    // -- eval_predicate ------------------------------------------------

    fn make_pred(field: &str, op: CompareOp, value: PredicateValue) -> crate::ir::Predicate {
        crate::ir::Predicate {
            field: field.into(),
            op,
            value,
        }
    }

    #[test]
    fn eval_pred_eq_case_sensitive() {
        let sym = make_symbol("foo", "function", 0);
        // Exact match: same case → true.
        let pred_match = make_pred(
            "fql_kind",
            CompareOp::Eq,
            PredicateValue::String("function".into()),
        );
        assert!(eval_predicate(&sym, &pred_match), "Eq same case must match");
        // Different case → false.
        let pred_no_match = make_pred(
            "fql_kind",
            CompareOp::Eq,
            PredicateValue::String("FUNCTION".into()),
        );
        assert!(
            !eval_predicate(&sym, &pred_no_match),
            "Eq different case must not match"
        );
    }

    #[test]
    fn eval_pred_noteq_matches_different_value() {
        let sym = make_symbol("foo", "struct", 0);
        let pred = make_pred(
            "fql_kind",
            CompareOp::NotEq,
            PredicateValue::String("function".into()),
        );
        assert!(eval_predicate(&sym, &pred));
    }

    #[test]
    fn eval_pred_like_absent_field_is_false() {
        // "signature" field does not exist on this symbol → Like returns false.
        let sym = make_symbol("foo", "function", 0);
        let pred = make_pred(
            "signature",
            CompareOp::Like,
            PredicateValue::String("%".into()),
        );
        assert!(!eval_predicate(&sym, &pred));
    }

    #[test]
    fn eval_pred_notlike_absent_field_is_false() {
        // NotLike with absent field: is_some_and returns false (not true).
        let sym = make_symbol("foo", "function", 0);
        let pred = make_pred(
            "signature",
            CompareOp::NotLike,
            PredicateValue::String("%".into()),
        );
        assert!(
            !eval_predicate(&sym, &pred),
            "NotLike on absent field must be false, not true"
        );
    }

    #[test]
    fn eval_pred_bool_eq_always_false() {
        let sym = make_symbol("foo", "function", 0);
        let pred = make_pred("name", CompareOp::Eq, PredicateValue::Bool(true));
        assert!(
            !eval_predicate(&sym, &pred),
            "Bool predicate with Eq must always return false"
        );
    }

    #[test]
    fn eval_pred_bool_noteq_always_false() {
        let sym = make_symbol("foo", "function", 0);
        let pred = make_pred("name", CompareOp::NotEq, PredicateValue::Bool(false));
        assert!(
            !eval_predicate(&sym, &pred),
            "Bool predicate with NotEq must always return false"
        );
    }

    #[test]
    fn eval_pred_gt_gte_lt_lte_numeric() {
        let sym = make_symbol("foo", "function", 5);
        assert!(eval_predicate(
            &sym,
            &make_pred("usages", CompareOp::Gt, PredicateValue::Number(4))
        ));
        assert!(eval_predicate(
            &sym,
            &make_pred("usages", CompareOp::Gte, PredicateValue::Number(5))
        ));
        assert!(eval_predicate(
            &sym,
            &make_pred("usages", CompareOp::Lt, PredicateValue::Number(6))
        ));
        assert!(eval_predicate(
            &sym,
            &make_pred("usages", CompareOp::Lte, PredicateValue::Number(5))
        ));
        assert!(!eval_predicate(
            &sym,
            &make_pred("usages", CompareOp::Gt, PredicateValue::Number(5))
        ));
    }

    #[test]
    fn eval_pred_numeric_absent_field_is_false() {
        let sym = SymbolMatch {
            name: "x".into(),
            node_kind: None,
            fql_kind: None,
            language: None,
            path: None,
            line: None,
            usages_count: None, // absent numeric field
            fields: HashMap::new(),
            count: None,
        };
        let pred = make_pred("usages", CompareOp::Gt, PredicateValue::Number(0));
        assert!(
            !eval_predicate(&sym, &pred),
            "Gt on absent numeric field must be false"
        );
    }

    #[test]
    fn eval_pred_matches_valid_regex() {
        let sym = make_symbol("init_motor", "function", 0);
        let pred = make_pred(
            "name",
            CompareOp::Matches,
            PredicateValue::String("^init_".into()),
        );
        assert!(eval_predicate(&sym, &pred));
    }

    #[test]
    fn eval_pred_matches_invalid_regex_is_false() {
        let sym = make_symbol("foo", "function", 0);
        let pred = make_pred(
            "name",
            CompareOp::Matches,
            PredicateValue::String("[invalid".into()),
        );
        assert!(
            !eval_predicate(&sym, &pred),
            "invalid regex must return false, not panic"
        );
    }

    #[test]
    fn eval_pred_notmatches_invalid_regex_is_true() {
        // NotMatches with invalid regex returns true (safe default — don't exclude).
        let sym = make_symbol("foo", "function", 0);
        let pred = make_pred(
            "name",
            CompareOp::NotMatches,
            PredicateValue::String("[invalid".into()),
        );
        assert!(
            eval_predicate(&sym, &pred),
            "invalid regex with NotMatches must return true"
        );
    }

    // -- apply_clauses gap tests ---------------------------------------

    #[test]
    fn apply_clauses_offset_skips_first_n() {
        let mut items: Vec<SymbolMatch> = ["a", "b", "c", "d", "e"]
            .iter()
            .map(|n| make_symbol(n, "function", 0))
            .collect();
        let clauses = Clauses {
            offset: Some(2),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].name, "c");
    }

    #[test]
    fn apply_clauses_offset_and_limit() {
        let mut items: Vec<SymbolMatch> = (0..8_u32)
            .map(|i| make_symbol(&i.to_string(), "function", 0))
            .collect();
        let clauses = Clauses {
            offset: Some(2),
            limit: Some(3),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].name, "2");
        assert_eq!(items[2].name, "4");
    }

    #[test]
    fn apply_clauses_offset_beyond_length_returns_empty() {
        let mut items = vec![make_symbol("a", "function", 0)];
        let clauses = Clauses {
            offset: Some(100),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert!(items.is_empty());
    }

    #[test]
    fn apply_clauses_group_by_injects_count() {
        // 3 functions + 1 struct → GROUP BY fql_kind → 2 groups with counts.
        let mut items = vec![
            make_symbol("a", "function", 0),
            make_symbol("b", "function", 0),
            make_symbol("c", "function", 0),
            make_symbol("d", "struct", 0),
        ];
        let clauses = Clauses {
            group_by: Some(crate::ir::GroupBy::Field("fql_kind".into())),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 2, "two groups expected");
        let func = items
            .iter()
            .find(|s| s.fql_kind.as_deref() == Some("function"))
            .unwrap();
        assert_eq!(func.count, Some(3), "function group count must be 3");
        let strct = items
            .iter()
            .find(|s| s.fql_kind.as_deref() == Some("struct"))
            .unwrap();
        assert_eq!(strct.count, Some(1), "struct group count must be 1");
    }

    #[test]
    fn apply_clauses_having_filters_after_group() {
        // HAVING count >= 2 removes singleton groups.
        let mut items = vec![
            make_symbol("a", "function", 0),
            make_symbol("b", "function", 0),
            make_symbol("c", "function", 0),
            make_symbol("d", "struct", 0),
        ];
        let clauses = Clauses {
            group_by: Some(crate::ir::GroupBy::Field("fql_kind".into())),
            having_predicates: vec![Predicate {
                field: "count".into(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(2),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].fql_kind.as_deref(), Some("function"));
    }

    #[test]
    fn apply_clauses_multiple_where_are_and() {
        // WHERE fql_kind = "function" AND name LIKE "init%"
        // Only "init_motor" should survive.
        let mut items = vec![
            make_symbol("init_motor", "function", 0),
            make_symbol("init_sensor", "struct", 0), // wrong kind
            make_symbol("run_motor", "function", 0), // wrong name
        ];
        let clauses = Clauses {
            where_predicates: vec![
                Predicate {
                    field: "fql_kind".into(),
                    op: CompareOp::Eq,
                    value: PredicateValue::String("function".into()),
                },
                Predicate {
                    field: "name".into(),
                    op: CompareOp::Like,
                    value: PredicateValue::String("init%".into()),
                },
            ],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "init_motor");
    }

    #[test]
    fn apply_clauses_order_by_tiebreaker_is_name() {
        // Two symbols with the same usages — secondary sort must be by name ASC.
        let mut items = vec![
            make_symbol("zebra", "function", 5),
            make_symbol("alpha", "function", 5),
            make_symbol("middle", "function", 5),
        ];
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "usages".into(),
                direction: crate::ir::SortDirection::Asc,
            }),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items[0].name, "alpha");
        assert_eq!(items[1].name, "middle");
        assert_eq!(items[2].name, "zebra");
    }

    #[test]
    fn apply_clauses_in_glob_no_match_returns_empty() {
        let mut items = vec![
            make_symbol("foo", "function", 0), // path: src/foo.cpp
        ];
        let clauses = Clauses {
            in_glob: Some("include/**".into()),
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert!(
            items.is_empty(),
            "IN glob that matches nothing must produce empty result"
        );
    }

    #[test]
    fn apply_clauses_exclude_combined_with_where() {
        // Exclude src/ paths, keep non-src. Then WHERE keeps only "function".
        // Both "src/foo.cpp" items are excluded, only "lib/bar.cpp" "function" survives.
        let mut items: Vec<SymbolMatch> = vec![
            {
                let mut s = make_symbol("foo", "function", 0);
                s.path = Some(PathBuf::from("src/foo.cpp"));
                s
            },
            {
                let mut s = make_symbol("bar", "function", 0);
                s.path = Some(PathBuf::from("lib/bar.cpp"));
                s
            },
            {
                let mut s = make_symbol("baz", "struct", 0);
                s.path = Some(PathBuf::from("lib/baz.cpp"));
                s
            },
        ];
        let clauses = Clauses {
            exclude_glob: Some("src/**".into()),
            where_predicates: vec![Predicate {
                field: "fql_kind".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("function".into()),
            }],
            ..Default::default()
        };
        apply_clauses(&mut items, &clauses);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "bar");
    }
}
