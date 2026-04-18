//! `parse_clauses`, `parse_predicate`, `parse_compare_op`.
use super::Rule;
use super::helpers::unquote;
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
                clauses.in_glob = pair.into_inner().next().map(|p| unquote(p.as_str()));
            }
            Rule::exclude_clause => {
                clauses.exclude_glob = pair.into_inner().next().map(|p| unquote(p.as_str()));
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
                clauses.group_by = pair
                    .into_inner()
                    .next()
                    .map(|p| GroupBy::Field(p.as_str().to_string()));
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
        Rule::any_value => PredicateValue::String(unquote(inner.as_str())),
        Rule::signed_number => PredicateValue::Number(inner.as_str().parse().unwrap_or(0)),
        Rule::boolean_literal => PredicateValue::Bool(inner.as_str() == "true"),
        _ => return None,
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
