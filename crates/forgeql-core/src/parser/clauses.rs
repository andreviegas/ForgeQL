//! `parse_clauses`, `parse_predicate`, `parse_compare_op`.
use super::Rule;
use super::helpers::unwrap_any_value;
use crate::ir::{Clauses, CompareOp, GroupBy, OrderBy, Predicate, PredicateValue, SortDirection};
pub(super) fn parse_clauses(pairs: pest::iterators::Pairs<'_, Rule>) -> Clauses {
    let mut clauses = Clauses::default();
    for pair in pairs {
        match pair.as_rule() {
            // `clauses` itself may be the single child — just recurse one level.
            Rule::clauses => {
                clauses = parse_clauses(pair.into_inner());
            }
            Rule::where_clause => {
                if let Some(predicate) = pair.into_inner().next().and_then(parse_predicate) {
                    clauses.where_predicates.push(predicate);
                }
            }
            Rule::having_clause => {
                if let Some(predicate) = pair.into_inner().next().and_then(parse_predicate) {
                    clauses.having_predicates.push(predicate);
                }
            }
            Rule::in_clause => {
                clauses.in_glob = pair
                    .into_inner()
                    .next()
                    .and_then(|p| unwrap_any_value(p).ok());
            }
            Rule::exclude_clause => {
                if let Some(glob) = pair
                    .into_inner()
                    .next()
                    .and_then(|p| unwrap_any_value(p).ok())
                {
                    clauses.exclude_globs.push(glob);
                }
            }
            Rule::order_clause => {
                // order_clause = { "ORDER" ~ "BY" ~ field_name ~ sort_dir? }
                let mut parts = pair.into_inner();
                let field = parts
                    .next()
                    .map_or_else(String::new, |p| p.as_str().to_string());
                let direction = parts.next().map_or(SortDirection::Desc, |d| {
                    if d.as_str() == "ASC" {
                        SortDirection::Asc
                    } else {
                        SortDirection::Desc
                    }
                });
                clauses.order_by = Some(OrderBy { field, direction });
            }
            Rule::group_clause => {
                clauses.group_by = pair.into_inner().next().map(|p| {
                    // The WRITTEN spelling is kept, deliberately. It is what
                    // labels the key column — `GROUP BY file` heads its column
                    // `file`, not `path` — and every consumer that has to
                    // agree about WHICH field this is puts it through
                    // `field_tiers::canonical` first, so one alias table
                    // serves both the routing and the label.
                    GroupBy::Field(p.as_str().to_string())
                });
            }
            Rule::limit_clause => {
                clauses.limit = pair
                    .into_inner()
                    .next()
                    .and_then(|n| n.as_str().parse().ok());
            }
            Rule::offset_clause => {
                clauses.offset = pair
                    .into_inner()
                    .next()
                    .and_then(|n| n.as_str().parse().ok());
            }
            Rule::depth_clause | Rule::lines_clause => {
                clauses.depth = pair
                    .into_inner()
                    .next()
                    .and_then(|n| n.as_str().parse().ok());
            }
            _ => {}
        }
    }
    clauses
}

/// Parse a `predicate` pair into a `Predicate`.
pub(super) fn parse_predicate(pair: pest::iterators::Pair<'_, Rule>) -> Option<Predicate> {
    if pair.as_rule() != Rule::predicate {
        return None;
    }
    let mut parts = pair.into_inner();
    let field = parts.next()?.as_str().to_string();
    let op = parse_compare_op(parts.next()?.as_str());
    // predicate_value = { signed_number | boolean_literal | any_value }
    let val_pair = parts.next()?;
    let inner = val_pair.into_inner().next()?;
    let value = match inner.as_rule() {
        Rule::any_value => PredicateValue::String(unwrap_any_value(inner).ok()?),
        Rule::signed_number => PredicateValue::Number(inner.as_str().parse().unwrap_or(0)),
        Rule::boolean_literal => PredicateValue::Bool(inner.as_str() == "true"),
        _ => return None,
    };

    // `fql_kind` is the one field an agent can spell two ways for one value: a
    // row nothing maps STORES the empty kind, and `SHOW outline` / `SHOW
    // members` RENDER that same row as `unknown`, so a filter written from what
    // the engine printed says `unknown` while one written from what it stores
    // says `''`. Both are accepted values, and they name the same rows, so the
    // value is spelled to the stored one here — the single place an agent's
    // text becomes an IR value, ahead of every verb, every index reader and
    // both storage backends. Doing it here rather than in each reader is what
    // keeps the two spellings from drifting apart one lookup at a time.
    //
    // Only `=` and `!=` are spelled. `LIKE` and `MATCHES` carry a PATTERN, not
    // a value; they are matched against the spelling the verb rendered.
    let value = match (&value, op) {
        (PredicateValue::String(s), CompareOp::Eq | CompareOp::NotEq)
            if crate::field_tiers::canonical(&field) == "fql_kind" =>
        {
            PredicateValue::String(crate::field_tiers::stored_kind_value(s).to_owned())
        }
        _ => value,
    };
    Some(Predicate { field, op, value })
}

/// Map a raw `compare_op` text to the typed enum.
///
/// Normalises any internal whitespace so that `"NOT  LIKE"` and `"NOT LIKE"`
/// both map to `CompareOp::NotLike`.
pub(super) fn parse_compare_op(op_str: &str) -> CompareOp {
    let normalised: String = op_str.split_whitespace().collect::<Vec<_>>().join(" ");
    match normalised.as_str() {
        "!=" => CompareOp::NotEq,
        "LIKE" => CompareOp::Like,
        "NOT LIKE" => CompareOp::NotLike,
        "MATCHES" => CompareOp::Matches,
        "NOT MATCHES" => CompareOp::NotMatches,
        ">" => CompareOp::Gt,
        ">=" => CompareOp::Gte,
        "<" => CompareOp::Lt,
        "<=" => CompareOp::Lte,
        // "=" and any unexpected token default to Eq.
        _ => CompareOp::Eq,
    }
}
