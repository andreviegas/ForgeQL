//! Parser tests for `WHERE` predicates: comparison operators, the value
//! forms a predicate accepts, and negative-number literals.
//!
//! An `ORDER BY` direction is asserted here only where a `WHERE` test happens
//! to carry one. The tests that are *about* ordering sit with the
//! `FIND globals` tests.

use crate::ir::{CompareOp, PredicateValue, SortDirection};
use crate::parser::*;

// ── WHERE predicates ────────────────────────────────────────────────────

#[test]
fn parse_find_symbols_usages_n() {
    let ops = parse("FIND symbols WHERE usages = 3").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "usages");
            assert_eq!(p.op, CompareOp::Eq);
            assert_eq!(p.value, PredicateValue::Number(3));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_where_name_like() {
    // LIKE predicate with a string value
    let ops = parse("FIND symbols WHERE name LIKE 'set%'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "name");
            assert_eq!(p.op, CompareOp::Like);
            assert_eq!(p.value, PredicateValue::String("set%".into()));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_where_name_not_like() {
    let ops = parse("FIND symbols WHERE name NOT LIKE 'test%'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::NotLike);
            assert_eq!(p.value, PredicateValue::String("test%".into()));
        }
        _ => panic!("wrong variant"),
    }
}

// ── Comparison operators in WHERE ────────────────────────────────────────

#[test]
fn parse_find_symbols_where_usages_gte() {
    let ops = parse("FIND symbols WHERE usages >= 5 ORDER BY usages DESC LIMIT 10").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "usages");
            assert_eq!(p.op, CompareOp::Gte);
            assert_eq!(p.value, PredicateValue::Number(5));
            let order = clauses.order_by.as_ref().expect("order_by should be Some");
            assert_eq!(order.field, "usages");
            assert_eq!(order.direction, SortDirection::Desc);
            assert_eq!(clauses.limit, Some(10));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_where_usages_not_eq() {
    let ops = parse("FIND symbols WHERE usages != 0").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::NotEq);
            assert_eq!(p.value, PredicateValue::Number(0));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_where_usages_lte() {
    let ops = parse("FIND symbols WHERE usages <= 10").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::Lte);
            assert_eq!(p.value, PredicateValue::Number(10));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_where_usages_gt() {
    let ops = parse("FIND symbols WHERE usages > 0 IN 'src/**'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::Gt);
            assert_eq!(p.value, PredicateValue::Number(0));
            assert_eq!(clauses.in_glob.as_deref(), Some("src/**"));
        }
        _ => panic!("wrong variant"),
    }
}

// ── Negative number literals in predicates ───────────────────────────────

#[test]
fn parse_where_usages_eq_negative_one() {
    let ops = parse("FIND symbols WHERE usages = -1").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "usages");
            assert_eq!(p.op, CompareOp::Eq);
            assert_eq!(p.value, PredicateValue::Number(-1));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_where_usages_gt_negative_one() {
    let ops = parse("FIND symbols WHERE usages > -1").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::Gt);
            assert_eq!(p.value, PredicateValue::Number(-1));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_where_line_gte_negative_number() {
    let ops = parse("FIND symbols WHERE line >= -100").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "line");
            assert_eq!(p.op, CompareOp::Gte);
            assert_eq!(p.value, PredicateValue::Number(-100));
        }
        _ => panic!("wrong variant"),
    }
}

// ── Relaxed quoting in predicate values ─────────────────────────────────

#[test]
fn parse_where_bare_value() {
    // WHERE field = bare_value (no quotes).
    let ops = parse("FIND symbols WHERE fql_kind = function").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "fql_kind");
            assert_eq!(p.value, PredicateValue::String("function".into()));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_where_double_quoted_value() {
    // WHERE field = "value" (double quotes).
    let ops = parse(r#"FIND symbols WHERE fql_kind = "function""#).unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "fql_kind");
            assert_eq!(p.value, PredicateValue::String("function".into()));
        }
        _ => panic!("wrong variant"),
    }
}

// -- comparison operator round-trips ----------------------------------

#[test]
fn parse_where_usages_lt() {
    let ops = parse("FIND symbols WHERE usages < 3").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::Lt);
            assert_eq!(p.value, PredicateValue::Number(3));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_where_name_matches() {
    let ops = parse("FIND symbols WHERE name MATCHES '^get_'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::Matches);
            assert_eq!(p.value, PredicateValue::String("^get_".into()));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_where_name_not_matches() {
    let ops = parse("FIND symbols WHERE name NOT MATCHES '^test_'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let p = &clauses.where_predicates[0];
            assert_eq!(p.op, CompareOp::NotMatches);
            assert_eq!(p.value, PredicateValue::String("^test_".into()));
        }
        _ => panic!("wrong variant"),
    }
}
