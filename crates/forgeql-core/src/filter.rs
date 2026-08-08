/// Universal clause pipeline for `ForgeQL` read-only operations.
///
/// Every list-returning query pipes its raw results through [`apply_clauses`],
/// which applies path inclusion/exclusion, WHERE predicates, GROUP BY,
/// HAVING predicates, ORDER BY, OFFSET, and LIMIT — in that fixed order.
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// What this row is called when a clause names a field it cannot carry.
    const ROW: &'static str;

    /// Field names [`field_str`](ClauseTarget::field_str) resolves.
    ///
    /// Canonical spellings only. A clause field is put through
    /// [`crate::field_tiers::canonical`] before it reaches a row, so an alias
    /// declared in `FIELD_TIERS` needs no entry here — and cannot be listed
    /// here without also being declared there.
    const STR_FIELDS: &'static [&'static str];

    /// Field names [`field_num`](ClauseTarget::field_num) resolves, on the
    /// same terms.
    const NUM_FIELDS: &'static [&'static str];

    /// Whether a name in neither list may still resolve on some row.
    ///
    /// True only for a row carrying an open enrichment map, where the set
    /// depends on which language plugins are registered and on what the
    /// segments actually stored — so whether an unlisted name can match is a
    /// question only the backend holding the index can answer, and
    /// [`reject_unresolvable_fields`] declines to answer it. False is the
    /// stronger claim: the two lists above are the whole universe, and any
    /// other name is refused on sight rather than matching nothing.
    const OPEN_FIELDS: bool;

    /// Fields this row carries whose value comes from the symbol the verb
    /// addressed rather than from the row itself.
    ///
    /// A `SHOW callees` row's `path` is the file the *call* sits in, and every
    /// call in one answer sits inside the single function that was resolved —
    /// so every row carries the same value, and filtering rows by it can only
    /// ever keep all of them or none. Written by an agent it means "the
    /// `shared_fn` in this file", which is a question for the lookup.
    ///
    /// Such a field is given to BOTH consumers: the lookup, which it genuinely
    /// narrows, and the rows, where it is a no-op once the lookup has honoured
    /// it. Membership here is not "the rows also carry this name" — `line` on
    /// a source line is carried by a symbol row too, and `SHOW body OF 'f'
    /// WHERE line > 10` means the lines, not a definition starting after line
    /// 10. It is the narrower claim that the row's value is a property of the
    /// resolved symbol.
    const LOOKUP_FIELDS: &'static [&'static str] = &[];
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

/// Every core (non-enrichment) WHERE field name, unioned across FIND / SHOW
/// result shapes.
///
/// It is a union, and that is the one thing to remember about it: no single
/// query answers all of these, so membership here says a name exists SOMEWHERE
/// and nothing about the verb in hand. Using it as a per-verb gate is what let
/// `FIND symbols WHERE size > 100` through to answer a confident zero — `size`
/// belongs to a file row. Each verb gates on the row shape it actually
/// returns: [`ClauseTarget::STR_FIELDS`] / [`ClauseTarget::NUM_FIELDS`] for the
/// closed shapes, and the columnar backend's own Stage 0 for symbol rows,
/// which alone can see the enrichment columns a segment stored.
///
/// What it is still for: the engine's empty-result hint, which asks only
/// whether a name is plausible anywhere before blaming a typo, and the
/// enrichment-bitmap guard in `fast_paths`, which must never read "no segment
/// stores a column by that name" as absence for a name served elsewhere.
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

/// Refuse a clause naming a field this row shape cannot resolve.
///
/// Only closed row shapes are checked — those whose [`ClauseTarget::STR_FIELDS`]
/// and [`ClauseTarget::NUM_FIELDS`] are the whole universe. A row carrying an
/// open enrichment map returns `Ok(())` here and is checked by the backend
/// holding the index, which is the only thing that can say whether an unlisted
/// name is stored by some segment.
///
/// The clause matters as much as the field. `apply_group_by` keys a row
/// through `field_str` alone, so a numeric-only field groups every row under
/// the empty string and reports one fabricated group holding the whole result;
/// `order_cmp` falls back to name order when a field resolves on no row, and
/// hands back alphabetical rows labelled "top N by <field>"; a `WHERE` on a
/// field `GROUP BY` has not written yet matches nothing at all. Three clauses,
/// three different shapes of wrong answer, one cause.
///
/// # Errors
///
/// Returns an error naming the field, the clause and the row shape when the
/// clause names something the shape cannot resolve. That is the contract: a
/// query that cannot be answered errors, and never returns zero rows.
pub fn reject_unresolvable_fields<T: ClauseTarget>(
    verb: &str,
    clauses: &Clauses,
) -> anyhow::Result<()> {
    if T::OPEN_FIELDS {
        return Ok(());
    }
    for pred in &clauses.where_predicates {
        check_clause_field::<T>(verb, "WHERE", &pred.field, ClauseKind::Where)?;
    }
    reject_unresolvable_shaping_fields::<T>(verb, clauses)
}

/// Refuse an `ORDER BY`, `GROUP BY` or `HAVING` naming a field this row shape
/// cannot resolve.
///
/// The half of [`reject_unresolvable_fields`] that applies to a verb whose
/// `WHERE` is shared with a symbol lookup. Those three clauses are not shared:
/// no resolver reads them, so they can only ever be answered from the returned
/// rows, and the row shape is the whole universe of names they may use — the
/// same standard a filter-only verb is held to. `WHERE` on such a verb is
/// split instead, by [`clauses_for_rows`] and [`clauses_for_lookup`].
///
/// # Errors
///
/// Returns an error naming the field, the clause and the row shape when the
/// clause names something the shape cannot resolve.
pub fn reject_unresolvable_shaping_fields<T: ClauseTarget>(
    verb: &str,
    clauses: &Clauses,
) -> anyhow::Result<()> {
    if T::OPEN_FIELDS {
        return Ok(());
    }
    if let Some(ref order) = clauses.order_by {
        check_clause_field::<T>(verb, "ORDER BY", &order.field, ClauseKind::AfterGrouping)?;
    }
    if let Some(GroupBy::Field(ref field)) = clauses.group_by {
        check_clause_field::<T>(verb, "GROUP BY", field, ClauseKind::Group)?;
    }
    for pred in &clauses.having_predicates {
        check_clause_field::<T>(verb, "HAVING", &pred.field, ClauseKind::AfterGrouping)?;
    }
    Ok(())
}

/// Which accessors a clause can reach a row through.
#[derive(Clone, Copy)]
enum ClauseKind {
    /// Runs before the grouping pass: both accessors, minus anything grouping
    /// is what writes.
    Where,
    /// Keys the row: `field_str` only.
    Group,
    /// Runs after the grouping pass: both accessors, grouping's own output
    /// included.
    AfterGrouping,
}

fn check_clause_field<T: ClauseTarget>(
    verb: &str,
    clause: &str,
    written: &str,
    kind: ClauseKind,
) -> anyhow::Result<()> {
    let field = crate::field_tiers::canonical(written);
    let post_group = is_post_group(field);
    let available: Vec<&str> = match kind {
        ClauseKind::Group => T::STR_FIELDS.to_vec(),
        ClauseKind::Where => T::STR_FIELDS
            .iter()
            .chain(T::NUM_FIELDS)
            .copied()
            .filter(|f| !is_post_group(f))
            .collect(),
        ClauseKind::AfterGrouping => T::STR_FIELDS.iter().chain(T::NUM_FIELDS).copied().collect(),
    };
    if available.contains(&field) {
        return Ok(());
    }
    // `count` is refused in one clause and answered in two, so the row's field
    // list cannot explain it — the table's own wording does.
    if post_group && let Some(tier) = crate::field_tiers::lookup(field) {
        anyhow::bail!("{}", tier.refusal(written, clause, verb));
    }
    anyhow::bail!(
        "{clause} {written} cannot be answered on {verb}: {row} carries no field \
         of that name, so the query could only report absence. Available: {list}.",
        row = T::ROW,
        list = available.join(", "),
    );
}

/// Whether the grouping pass is what writes this field onto a row.
fn is_post_group(field: &str) -> bool {
    crate::field_tiers::lookup(field).is_some_and(|tier| tier.post_group)
}

/// Refuse a clause naming a field the table itself declares unanswerable.
///
/// This is the check for the verbs whose clause is NOT only a row filter.
/// `SHOW members`, `SHOW callees` and the reading verbs pass one clause to two
/// consumers — the lookup that picks which symbol `OF` names, and the rows that
/// come back — so neither shape alone is the universe of legitimate names, and
/// the row-shape check would refuse `SHOW members OF 'Foo' WHERE language =
/// 'cpp'`, the documented way to disambiguate a name across languages. What is
/// refused here is only what NEITHER consumer can answer: the
/// [`crate::field_tiers::refused_fields`] set — `node_kind`, whose value
/// nothing in the index stores, and the names belonging to some third shape
/// (`size`, `depth`, `extension`, `signature`, `marker`, `declaration`) that
/// reached these verbs only because `CORE_WHERE_FIELDS` is a union.
///
/// It runs on the whole clause, before [`clauses_for_rows`] and
/// [`clauses_for_lookup`] split the `WHERE` between the two consumers. That
/// order is deliberate: a refused name would otherwise fall to the lookup half
/// and be reported as a symbol nothing matched, when the honest answer is that
/// the field cannot be asked about at all.
///
/// Minus what THIS shape carries, which is the other half of the rule. The
/// table's verdict is about symbol rows, and several of those names are the
/// canonical field of some other shape: `text` and `marker` on a source line,
/// `declaration` on a members row. Refusing `SHOW body OF 'f' WHERE text
/// MATCHES '…'` because a symbol row has no `text` would refuse the documented
/// way to use the verb. The exemption applies only to a CLOSED shape — an open
/// one cannot say what it carries, and `node_kind` is listed on a symbol row
/// precisely because the legacy backend resolves it.
///
/// Reading the set from the table rather than restating it is the point: a name
/// added there is refused everywhere without a second edit, which is how this
/// family grew unnoticed in the first place.
///
/// All four clauses are checked, and for the same reason — none can be answered
/// from a field no row carries — but each fails a different way, so each is
/// worth refusing separately. `WHERE` matches nothing while its negation
/// matches everything; `ORDER BY` ties every row and silently falls back to
/// name order; `GROUP BY` keys every row to the empty string and reports one
/// fabricated group whose count is the whole result set.
///
/// `count` is the one name whose answer depends on the clause: the grouping
/// pass is what writes it, so `HAVING count` and `ORDER BY count` read a real
/// number while a `WHERE` on it reads nothing on every row.
///
/// # Errors
///
/// Returns the table's own refusal wording for the first clause field that is
/// declared unanswerable.
pub fn reject_refused_fields<T: ClauseTarget>(verb: &str, clauses: &Clauses) -> anyhow::Result<()> {
    for pred in &clauses.where_predicates {
        reject_if_refused::<T>(&pred.field, "WHERE", verb, BeforeGrouping::Yes)?;
    }
    if let Some(ref order) = clauses.order_by {
        reject_if_refused::<T>(&order.field, "ORDER BY", verb, BeforeGrouping::No)?;
    }
    if let Some(GroupBy::Field(ref field)) = clauses.group_by {
        reject_if_refused::<T>(field, "GROUP BY", verb, BeforeGrouping::Yes)?;
    }
    for pred in &clauses.having_predicates {
        reject_if_refused::<T>(&pred.field, "HAVING", verb, BeforeGrouping::No)?;
    }
    Ok(())
}

/// Whether the clause runs before the grouping pass that writes `count`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BeforeGrouping {
    Yes,
    No,
}

fn reject_if_refused<T: ClauseTarget>(
    written: &str,
    clause: &str,
    verb: &str,
    before_grouping: BeforeGrouping,
) -> anyhow::Result<()> {
    let field = crate::field_tiers::canonical(written);
    // What this shape carries, it can answer — whatever the table says about
    // the shape the table describes. Only a closed shape may claim this: an
    // open one does not know its own field set.
    if !T::OPEN_FIELDS && (T::STR_FIELDS.contains(&field) || T::NUM_FIELDS.contains(&field)) {
        return Ok(());
    }
    let Some(tier) = crate::field_tiers::lookup(written) else {
        return Ok(());
    };
    if tier.post_group {
        return if before_grouping == BeforeGrouping::Yes {
            Err(anyhow::anyhow!(tier.refusal(written, clause, verb)))
        } else {
            Ok(())
        };
    }
    if tier.is_refused() {
        anyhow::bail!("{}", tier.refusal(written, clause, verb));
    }
    Ok(())
}

// -----------------------------------------------------------------------
// Splitting a clause between the lookup and the rows
// -----------------------------------------------------------------------

/// Whether a clause naming `written` can be answered from a `T` row.
///
/// An open shape answers `true` for every name: it cannot enumerate itself, so
/// it never claims a name is beyond it.
fn answers_on<T: ClauseTarget>(written: &str) -> bool {
    if T::OPEN_FIELDS {
        return true;
    }
    let field = crate::field_tiers::canonical(written);
    T::STR_FIELDS.contains(&field) || T::NUM_FIELDS.contains(&field)
}

/// The half of a `SHOW … OF 'name'` `WHERE` clause the returned rows answer.
///
/// `SHOW members`, `SHOW callees` and the reading verbs hand one clause to two
/// consumers: the lookup that decides which symbol `OF` names, and the rows
/// that come back from it. The two carry different fields, and giving the whole
/// clause to both is what made `SHOW members OF 'Foo' WHERE language = 'cpp'`
/// answer zero for a C++ `Foo` — the lookup ignored the predicate and then
/// every members row, which carries no `language`, failed it.
///
/// So each predicate goes to the consumer that can answer it, decided by the
/// row shape: a name the rows carry filters the rows, and a name they do not
/// carry is aimed at the symbol and travels to the lookup as
/// [`clauses_for_lookup`]. Together the two halves are the whole clause — no
/// predicate is dropped. The one name that reaches both is a
/// [`ClauseTarget::LOOKUP_FIELDS`] entry, whose value on every row is a
/// property of the resolved symbol: applying it to the rows after the lookup
/// honoured it is a no-op, and NOT giving it to the lookup left
/// `SHOW callees OF 'f' WHERE path = '…'` answering zero whenever the file
/// named was not the one the lookup happened to pick.
///
/// `WHERE` and the globs. `ORDER BY`, `GROUP BY` and `HAVING` shape the answer
/// and never the lookup, which reads none of them, so they stay whole and are
/// checked against the row shape by [`reject_unresolvable_shaping_fields`].
#[must_use]
pub fn clauses_for_rows<T: ClauseTarget>(clauses: &Clauses) -> Clauses {
    let mut out = clauses.clone();
    out.where_predicates.retain(|p| answers_on::<T>(&p.field));
    // `IN` and `EXCLUDE` are a statement about a file, and a row with no file
    // of its own cannot be the one they are about: a members row and a source
    // line both report `None` for their path, so retaining the globs here
    // dropped every row and `SHOW members OF 'Foo' IN 'crates/**'` answered
    // zero for a type that lives there. The globs stay in the lookup half,
    // which is what they were always describing — the file the symbol is in.
    if !answers_on::<T>("path") {
        out.in_glob = None;
        out.exclude_globs.clear();
    }
    out
}

/// The half of the same clause only the addressed symbol answers.
///
/// The complement of [`clauses_for_rows`], and the thing the storage engine's
/// `resolve_*` methods are given. A candidate must satisfy every predicate
/// handed over here, so a name no candidate row carries excludes them all and
/// the lookup fails — which is reported as a lookup that matched nothing, not
/// as an empty answer.
///
/// `ORDER BY`, `GROUP BY` and `HAVING` are stripped rather than passed through:
/// no resolver reads them, and leaving them on a value named "what the lookup
/// gets" would suggest otherwise.
#[must_use]
pub fn clauses_for_lookup<T: ClauseTarget>(clauses: &Clauses) -> Clauses {
    let mut out = clauses.clone();
    out.where_predicates.retain(|p| {
        !answers_on::<T>(&p.field)
            || T::LOOKUP_FIELDS.contains(&crate::field_tiers::canonical(&p.field))
    });
    out.order_by = None;
    out.group_by = None;
    out.having_predicates.clear();
    out
}

/// Render a predicate list back as the clause an agent wrote, for a message
/// that has to say which filter excluded everything.
#[must_use]
pub fn describe_predicates(predicates: &[crate::ir::Predicate]) -> String {
    predicates
        .iter()
        .map(|p| {
            let op = match p.op {
                CompareOp::Eq => "=",
                CompareOp::NotEq => "!=",
                CompareOp::Like => "LIKE",
                CompareOp::NotLike => "NOT LIKE",
                CompareOp::Matches => "MATCHES",
                CompareOp::NotMatches => "NOT MATCHES",
                CompareOp::Gt => ">",
                CompareOp::Gte => ">=",
                CompareOp::Lt => "<",
                CompareOp::Lte => "<=",
            };
            match &p.value {
                PredicateValue::String(s) => format!("WHERE {} {op} '{s}'", p.field),
                PredicateValue::Number(n) => format!("WHERE {} {op} {n}", p.field),
                PredicateValue::Bool(b) => format!("WHERE {} {op} {b}", p.field),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
    // An alias is spelled to its canonical name once, here, where the field
    // name meets the row. Every caller reaches a row through this function, so
    // the resolvers below — and the eight `field_str` implementations — only
    // ever see canonical names, and an alias added to `FIELD_TIERS` needs no
    // second entry anywhere.
    let field = crate::field_tiers::canonical(&predicate.field);
    match predicate.op {
        // ---- String / LIKE operators ----
        CompareOp::Like => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return false,
            };
            item.field_str(field).is_some_and(|v| like_match(v, pat))
        }
        CompareOp::NotLike => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return true,
            };
            item.field_str(field).is_some_and(|v| !like_match(v, pat))
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
            item.field_str(field).is_some_and(|v| re.is_match(v))
        }
        CompareOp::NotMatches => {
            let pat = match &predicate.value {
                PredicateValue::String(s) => s.as_str(),
                _ => return true,
            };
            let Ok(re) = Regex::new(pat) else {
                return true;
            };
            item.field_str(field).is_some_and(|v| !re.is_match(v))
        }
        CompareOp::Eq => match &predicate.value {
            PredicateValue::String(s) => item.field_str(field).is_some_and(|v| v == s.as_str()),
            PredicateValue::Number(n) => item.field_num(field).is_some_and(|v| v == *n),
            PredicateValue::Bool(_) => false,
        },
        CompareOp::NotEq => match &predicate.value {
            PredicateValue::String(s) => item.field_str(field).is_some_and(|v| v != s.as_str()),
            PredicateValue::Number(n) => item.field_num(field).is_some_and(|v| v != *n),
            PredicateValue::Bool(_) => false,
        },
        // ---- Numeric operators ----
        CompareOp::Gt => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(field).is_some_and(|v| v > rhs)),
        CompareOp::Gte => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(field).is_some_and(|v| v >= rhs)),
        CompareOp::Lt => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(field).is_some_and(|v| v < rhs)),
        CompareOp::Lte => numeric_rhs(&predicate.value)
            .is_some_and(|rhs| item.field_num(field).is_some_and(|v| v <= rhs)),
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
        let field = crate::field_tiers::canonical(&order_by.field);
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

/// How many site rows one `FIND usages` response renders before it starts
/// withholding whole files.
///
/// Selecting whole files bounds the *file* count but not the response: a hot
/// identifier can hold hundreds of sites in each of twenty files. Measured on
/// the frozen corpora, a realistic rename campaign returns ~130 sites across
/// its twenty files and the largest single file anywhere holds ~700, while a
/// query on a hot local name reaches several thousand — the shape this bounds.
/// Set well clear of honest use so it only ever trims the runaway case.
///
/// Public so a test can size a fixture from it rather than from a literal: a
/// test that hardcodes the number stops testing this constant the moment it is
/// retuned, and one that never reads it cannot notice a call site wired to a
/// different value.
pub const USAGE_SITE_CEILING: usize = 2_000;

/// The outcome of [`take_file_groups`]: the rows to render, and whether files
/// were left out.
pub(crate) struct FileGroups<T> {
    /// Every site of every selected file, in file order.
    pub rows: Vec<T>,
    /// Files that matched but were not rendered, and why. `None` when the
    /// selection is complete.
    pub withheld: Option<Withheld>,
}

/// Why a file that matched did not make it into the response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Withheld {
    /// More files matched than `LIMIT` (or the default cap) allows.
    Limit,
    /// The selected files together hold more sites than the response renders.
    Ceiling,
}

/// Limit a row list by *file group* rather than by row count.
///
/// A `FIND usages` row is one line of one file, and the question behind the
/// query is "which files hold this name?".  A cap counted in rows answers a
/// different question: it cuts the list mid-file, so a file reports part of its
/// sites and hides the rest with nothing to say it did.  The cap therefore
/// selects whole files — every site of a selected file is returned.
///
/// Groups keep the order in which their first row appears, which under the
/// default `(name, line, path)` ordering is lowest line first, and `OFFSET`
/// skips whole groups so paging never splits one file across two pages.
///
/// `ceiling` then bounds the total sites rendered, still dropping only whole
/// files and only from the tail, so file order never changes. The first
/// selected file is always rendered complete however large it is: a listing
/// that shows nothing at all would be worse than a long one, and the caller
/// learns the set was trimmed from [`FileGroups::withheld`].
///
/// It is a stop condition, not a fill target. A small first file followed by a
/// very large second one renders just the small file and stops, well short of
/// `ceiling` — skipping ahead to a file that fits is what "whole files, from
/// the tail" rules out, because it would silently perforate the order the
/// caller was promised.
pub(crate) fn take_file_groups<T: ClauseTarget>(
    results: Vec<T>,
    offset: usize,
    limit: usize,
    ceiling: usize,
) -> FileGroups<T> {
    let mut slot_of: HashMap<Option<PathBuf>, usize> = HashMap::new();
    let mut groups: Vec<Vec<T>> = Vec::new();
    for item in results {
        let key = item.path().map(Path::to_path_buf);
        let slot = *slot_of.entry(key).or_insert_with(|| {
            let next = groups.len();
            groups.push(Vec::new());
            next
        });
        groups[slot].push(item);
    }

    let matched = groups.len();
    let candidates: Vec<Vec<T>> = groups.into_iter().skip(offset).take(limit).collect();
    let over_limit = matched.saturating_sub(offset) > candidates.len();

    let mut rows: Vec<T> = Vec::new();
    for (taken, group) in candidates.into_iter().enumerate() {
        // The first file is always rendered whole — a response that shows no
        // file at all answers nothing. After that, a file is taken only if it
        // fits, and the first that does not ends the listing: dropping from the
        // tail is what keeps file order intact.
        if taken > 0 && rows.len() + group.len() > ceiling {
            return FileGroups {
                rows,
                withheld: Some(Withheld::Ceiling),
            };
        }
        rows.extend(group);
    }

    FileGroups {
        rows,
        withheld: over_limit.then_some(Withheld::Limit),
    }
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
    // Canonical, for the same reason `eval_predicate` canonicalises: the key
    // is read through `field_str`, and two spellings of one field must key the
    // same groups. The WRITTEN spelling still labels the column — `GROUP BY
    // file` heads its key column `file`, not `path`.
    let field = crate::field_tiers::canonical(field).to_owned();
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
