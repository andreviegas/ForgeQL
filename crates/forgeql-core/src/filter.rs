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

/// Core (non-enrichment) WHERE field names across FIND / SHOW result shapes.
///
/// One shared universe so the engine's empty-result hint and the columnar
/// backend's unknown-field guard can never disagree about which fields exist.
pub const CORE_WHERE_FIELDS: &[&str] = &[
    "name",
    "fql_kind",
    "kind",
    "node_kind",
    "node_id",
    "path",
    "file",
    "line",
    "usages",
    "count",
    "language",
    "lang",
    "extension",
    "ext",
    "size",
    "depth",
    "signature",
    "value",
    "type",
    "body",
    "text",
    "content",
    "marker",
    "declaration",
];

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
    apply_clauses_inner(results, clauses, true);
}

/// Like [`apply_clauses`] but keeps the caller's insertion order when there is
/// no explicit `ORDER BY`.
///
/// `SHOW outline` relies on this: its pre-order DFS sequence is the meaningful
/// default order, and the usual `(name, line, path)` tie-break sort would
/// flatten the structural tree into an alphabetical list.
pub fn apply_clauses_keep_order<T: ClauseTarget>(results: &mut Vec<T>, clauses: &Clauses) {
    apply_clauses_inner(results, clauses, false);
}

fn apply_clauses_inner<T: ClauseTarget>(
    results: &mut Vec<T>,
    clauses: &Clauses,
    default_sort: bool,
) {
    // 1. IN glob
    if let Some(ref glob) = clauses.in_glob {
        results.retain(|item| item.path().is_some_and(|p| path_glob_matches(p, glob)));
    }

    // 2. EXCLUDE globs — a row is dropped when ANY pattern matches its path.
    for glob in &clauses.exclude_globs {
        results.retain(|item| item.path().is_none_or(|p| !path_glob_matches(p, glob)));
    }

    // 3. WHERE predicates
    apply_where_predicates(results, &clauses.where_predicates);

    // 4. GROUP BY — deduplicate by group key and store per-group count in .count
    apply_group_by(results, clauses);

    // 5. HAVING predicates
    for predicate in &clauses.having_predicates {
        let pred = predicate.clone();
        results.retain(|item| eval_predicate(item, &pred));
    }

    // 6-8. ORDER BY (+ top-K fast path), then OFFSET and LIMIT.
    apply_ordering(results, clauses, default_sort);
}

/// Apply WHERE predicates with compile-once MATCHES / NOT MATCHES handling.
///
/// `.{N,}` collapses to a `len >= N` check, and every other regex pattern is
/// compiled once per predicate (not once per item) to avoid millions of
/// redundant regex compilations on large symbol tables (e.g. a 29 M+ symbol
/// kernel).
///
/// Public so storage backends can run the same compile-once residual filter
/// per segment (bounding memory to matching rows) before the final
/// [`apply_clauses`] pass — AND semantics make the early pass idempotent.
pub fn apply_where_predicates<T: ClauseTarget>(
    results: &mut Vec<T>,
    predicates: &[crate::ir::Predicate],
) {
    for predicate in predicates {
        if let (CompareOp::Matches | CompareOp::NotMatches, PredicateValue::String(pat)) =
            (&predicate.op, &predicate.value)
        {
            let is_matches = predicate.op == CompareOp::Matches;
            let field = predicate.field.clone();

            // Fast path: ".{N,}" ↔ len >= N (no newlines assumed in the target
            // field, which holds for structural enrichment values such as
            // condition_text, signature, and name).
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
                    // Invalid regex: MATCHES → nothing passes; NOT MATCHES → all
                    // pass (a no-op retain).
                    if is_matches {
                        results.clear();
                    }
                }
            }
        } else {
            let pred = predicate.clone();
            results.retain(|item| eval_predicate(item, &pred));
        }
    }
}

/// Apply GROUP BY: collapse to the first row per group key, recording the
/// per-group count on the kept row.
fn apply_group_by<T: ClauseTarget>(results: &mut Vec<T>, clauses: &Clauses) {
    let Some(GroupBy::Field(ref field)) = clauses.group_by else {
        return;
    };
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

/// Apply the final ordering pipeline: ORDER BY (with a bounded top-K fast path),
/// then OFFSET and LIMIT.  A deterministic order is established before
/// truncation so backends (legacy ↔ columnar) pick identical rows; even without
/// an explicit ORDER BY a stable `(name, line, path)` sort is applied.
fn apply_ordering<T: ClauseTarget>(results: &mut Vec<T>, clauses: &Clauses, default_sort: bool) {
    // Fast path: ORDER BY present, LIMIT <= TOPK_THRESHOLD, OFFSET zero, no
    // GROUP BY → `collect_top_k` (introselect O(N) avg) instead of an O(N log N)
    // sort; byte-identical via the shared `order_cmp` comparator.
    let want_topk = clauses.order_by.is_some()
        && clauses.group_by.is_none()
        && clauses.offset.unwrap_or(0) == 0
        && clauses.limit.is_some_and(|k| k <= TOPK_THRESHOLD);

    if let (Some(k), true) = (clauses.limit, want_topk) {
        let taken = std::mem::take(results);
        *results = collect_top_k(taken, k, |a, b| order_cmp(a, b, clauses));
        return; // OFFSET == 0 and LIMIT already applied by collect_top_k.
    }

    // Default tie-break sort (name, line, path) runs unless the caller asked to
    // preserve insertion order and supplied no explicit ORDER BY.
    if default_sort || clauses.order_by.is_some() {
        results.sort_by(|a, b| order_cmp(a, b, clauses));
    }

    // OFFSET
    let skip = clauses.offset.unwrap_or(0);
    if skip > 0 {
        let drained = skip.min(results.len());
        drop(results.drain(..drained));
    }

    // LIMIT
    if let Some(max) = clauses.limit {
        results.truncate(max);
    }
}

#[cfg(test)]
mod tests;
