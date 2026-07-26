//! Parser tests for the `FIND` verbs: `symbols`, `usages`, `files` and
//! `globals`, together with the `IN` and `EXCLUDE` path filters.
//!
//! Tests that use `FIND symbols` only as a carrier for a `WHERE` predicate
//! live in `clauses.rs` — what they assert is the clause, not the verb.

use crate::ir::{CompareOp, PredicateValue, SortDirection};
use crate::parser::*;

#[test]
fn parse_find_symbols() {
    let ops = parse("FIND symbols WHERE name LIKE 'set%' IN 'src/**/*.cpp'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "name");
            assert_eq!(p.op, CompareOp::Like);
            assert_eq!(p.value, PredicateValue::String("set%".into()));
            assert_eq!(clauses.in_glob.as_deref(), Some("src/**/*.cpp"));
            assert!(clauses.exclude_globs.is_empty());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_with_exclude() {
    let ops = parse("FIND symbols WHERE name LIKE 'set%' EXCLUDE 'tests/**'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(clauses.where_predicates.len(), 1);
            let p = &clauses.where_predicates[0];
            assert_eq!(p.field, "name");
            assert_eq!(p.op, CompareOp::Like);
            assert_eq!(p.value, PredicateValue::String("set%".into()));
            assert!(clauses.in_glob.is_none());
            assert_eq!(clauses.exclude_globs, vec!["tests/**".to_string()]);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_with_multiple_excludes_collects_all() {
    // BUG-017: the grammar accepts N EXCLUDE clauses; all must be honored
    // (previously Clauses.exclude_glob was an Option and the last one won).
    let ops = parse(
        "FIND symbols WHERE name LIKE 'set%' EXCLUDE 'crates/a/tests/**' EXCLUDE 'crates/b/tests/**'",
    )
    .unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            assert_eq!(
                clauses.exclude_globs,
                vec![
                    "crates/a/tests/**".to_string(),
                    "crates/b/tests/**".to_string()
                ]
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_usages_with_exclude() {
    let ops = parse("FIND usages OF 'showCode' EXCLUDE 'tests/**'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindUsages { of, clauses, .. } => {
            assert_eq!(of, "showCode");
            assert_eq!(clauses.exclude_globs, vec!["tests/**".to_string()]);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_files_no_in() {
    // FIND files without IN is now valid (scans all workspace files).
    let ops = parse("FIND files").unwrap();
    assert!(matches!(ops[0], ForgeQLIR::FindFiles { .. }));
}

#[test]
fn parse_find_files() {
    let ops = parse("FIND files IN 'include/**'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindFiles { clauses, .. } => {
            assert_eq!(clauses.in_glob.as_deref(), Some("include/**"));
            assert!(clauses.exclude_globs.is_empty());
            assert!(clauses.depth.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_files_with_exclude() {
    let ops = parse("FIND files IN 'src/**' EXCLUDE 'src/legacy/**'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindFiles { clauses, .. } => {
            assert_eq!(clauses.in_glob.as_deref(), Some("src/**"));
            assert_eq!(clauses.exclude_globs, vec!["src/legacy/**".to_string()]);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_files_with_depth() {
    let ops = parse("FIND files IN 'src/**' DEPTH 1").unwrap();
    match &ops[0] {
        ForgeQLIR::FindFiles { clauses, .. } => {
            assert_eq!(clauses.in_glob.as_deref(), Some("src/**"));
            assert_eq!(clauses.depth, Some(1));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_files_with_depth_and_exclude() {
    let ops = parse("FIND files IN 'src/**' EXCLUDE 'src/legacy/**' DEPTH 0").unwrap();
    match &ops[0] {
        ForgeQLIR::FindFiles { clauses, .. } => {
            assert_eq!(clauses.in_glob.as_deref(), Some("src/**"));
            assert_eq!(clauses.exclude_globs, vec!["src/legacy/**".to_string()]);
            assert_eq!(clauses.depth, Some(0));
        }
        _ => panic!("wrong variant"),
    }
}

// ── FIND globals / ORDER BY / LIMIT ─────────────────────────────────────

#[test]
fn parse_find_globals_sets_globals_only() {
    let ops = parse("FIND globals").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let kind_pred = clauses
                .where_predicates
                .iter()
                .find(|p| p.field == "fql_kind");
            assert!(
                kind_pred.is_some(),
                "globals should add a fql_kind predicate"
            );
            let kp = kind_pred.unwrap();
            assert_eq!(kp.op, CompareOp::Eq);
            assert_eq!(kp.value, PredicateValue::String("variable".into()));

            let scope_pred = clauses.where_predicates.iter().find(|p| p.field == "scope");
            assert!(scope_pred.is_some(), "globals should add a scope predicate");
            let sp = scope_pred.unwrap();
            assert_eq!(sp.op, CompareOp::Eq);
            assert_eq!(sp.value, PredicateValue::String("file".into()));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_globals_order_by_usages_desc_limit() {
    let ops = parse("FIND globals ORDER BY usages DESC LIMIT 20").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let kind_pred = clauses
                .where_predicates
                .iter()
                .find(|p| p.field == "fql_kind");
            assert!(
                kind_pred.is_some(),
                "globals should add a fql_kind predicate"
            );
            let scope_pred = clauses.where_predicates.iter().find(|p| p.field == "scope");
            assert!(scope_pred.is_some(), "globals should add a scope predicate");
            let order = clauses.order_by.as_ref().expect("order_by should be Some");
            assert_eq!(order.field, "usages");
            assert_eq!(order.direction, SortDirection::Desc);
            assert_eq!(clauses.limit, Some(20));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_globals_limit_and_offset() {
    let ops = parse("FIND globals ORDER BY name LIMIT 50 OFFSET 50").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { clauses, .. } => {
            let kind_pred = clauses
                .where_predicates
                .iter()
                .find(|p| p.field == "fql_kind");
            assert!(
                kind_pred.is_some(),
                "globals should add a fql_kind predicate"
            );
            let scope_pred = clauses.where_predicates.iter().find(|p| p.field == "scope");
            assert!(scope_pred.is_some(), "globals should add a scope predicate");
            let order = clauses.order_by.as_ref().expect("order_by should be Some");
            assert_eq!(order.field, "name");
            assert_eq!(order.direction, SortDirection::Desc); // default
            assert_eq!(clauses.limit, Some(50));
            assert_eq!(clauses.offset, Some(50));
        }
        _ => panic!("wrong variant"),
    }
}

// -- missing error path tests -----------------------------------------

#[test]
fn parse_find_unknown_target_is_error() {
    assert!(
        parse("FIND everything").is_err(),
        "FIND with unknown target should be a parse error"
    );
}
