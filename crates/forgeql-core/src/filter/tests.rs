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
        node_id: None,
        rev: None,
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
            node_id: None,
            rev: None,
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
            node_id: None,
            rev: None,
        },
    ];
    let clauses = Clauses {
        exclude_globs: vec!["tests/**".into()],
        ..Default::default()
    };
    apply_clauses(&mut items, &clauses);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "a");
}

#[test]
fn apply_clauses_multiple_exclude_globs_all_applied() {
    // BUG-017 regression: a row is dropped when ANY exclude glob matches.
    let mut items: Vec<SymbolMatch> = vec![
        {
            let mut s = make_symbol("a_test", "function", 0);
            s.path = Some(PathBuf::from("crates/a/tests/x.rs"));
            s
        },
        {
            let mut s = make_symbol("b_test", "function", 0);
            s.path = Some(PathBuf::from("crates/b/tests/y.rs"));
            s
        },
        {
            let mut s = make_symbol("keeper", "function", 0);
            s.path = Some(PathBuf::from("crates/a/src/z.rs"));
            s
        },
    ];
    let clauses = Clauses {
        exclude_globs: vec!["crates/a/tests/**".into(), "crates/b/tests/**".into()],
        ..Default::default()
    };
    apply_clauses(&mut items, &clauses);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "keeper");
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
            node_id: None,
            rev: None,
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
            node_id: None,
            rev: None,
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
            rev: None,
            line: 10,
            text: "void setup() {".into(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
        SourceLine {
            rev: None,
            line: 11,
            text: "    // TODO: fix this".into(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
        SourceLine {
            rev: None,
            line: 12,
            text: "    int x = 42;".into(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
        SourceLine {
            rev: None,
            line: 13,
            text: "    // FIXME: needs review".into(),
            marker: None,
            node_id: None,
            node_offset: None,
        },
        SourceLine {
            rev: None,
            line: 14,
            text: "}".into(),
            marker: None,
            node_id: None,
            node_offset: None,
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
        node_id: None,
        rev: None,
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
        exclude_globs: vec!["src/**".into()],
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

// -----------------------------------------------------------------------
// FileEntry field resolution (FIND files predicates)
// -----------------------------------------------------------------------

fn make_file_entry(path: &str, size: u64) -> crate::result::FileEntry {
    let path = PathBuf::from(path);
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    crate::result::FileEntry {
        path,
        depth: None,
        extension,
        size,
        count: None,
        error_count: None,
        parse_coverage: None,
        node_id: None,
        rev: None,
    }
}

#[test]
fn file_entry_where_name_matches_bare_file_name() {
    // `FIND files WHERE name = 'Kconfig'` — the idiomatic first guess.
    let mut items = vec![
        make_file_entry("kernel/Kconfig", 100),
        make_file_entry("kernel/Kconfig.smp", 200),
        make_file_entry("kernel/sched.c", 300),
    ];
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "name".into(),
            op: CompareOp::Eq,
            value: PredicateValue::String("Kconfig".into()),
        }],
        ..Default::default()
    };
    apply_clauses(&mut items, &clauses);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, PathBuf::from("kernel/Kconfig"));
}

#[test]
fn file_entry_has_error_is_none_until_the_query_asks() {
    // `error_count: None` means *not asked for*, never *no errors*.  An
    // unpopulated entry must match NEITHER `= 'true'` NOR `= 'false'`, or a
    // plain `FIND files` would silently report every file as clean.
    let entry = make_file_entry("kernel/sched.c", 300);
    assert_eq!(entry.field_str("has_error"), None);
    assert_eq!(entry.field_num("error_count"), None);
}

#[test]
fn file_entry_has_error_reflects_root_regions_only() {
    // `error_count` counts ONLY `error_scope = 'root'` regions — files that did
    // not parse as their declared language. A file full of `nested` regions
    // (macro-heavy C: Zephyr has 16 480 of them) is healthy and must read false.
    let mut healthy = make_file_entry("kernel/sched.c", 300);
    healthy.error_count = Some(0);
    assert_eq!(healthy.field_str("has_error"), Some("false"));
    assert_eq!(healthy.field_num("error_count"), Some(0));

    let mut unparsed = make_file_entry("data/actually_xml.c", 300);
    unparsed.error_count = Some(3);
    assert_eq!(unparsed.field_str("has_error"), Some("true"));
    assert_eq!(unparsed.field_num("error_count"), Some(3));
}

#[test]
fn file_entry_parse_coverage_is_a_percent() {
    let mut entry = make_file_entry("kernel/sched.c", 300);
    assert_eq!(entry.field_num("parse_coverage"), None);

    entry.parse_coverage = Some(99);
    assert_eq!(entry.field_num("parse_coverage"), Some(99));

    entry.parse_coverage = Some(0);
    assert_eq!(entry.field_num("parse_coverage"), Some(0));
}

#[test]
fn file_entry_where_name_like_matches_pattern() {
    let mut items = vec![
        make_file_entry("kernel/Kconfig", 100),
        make_file_entry("kernel/Kconfig.smp", 200),
        make_file_entry("kernel/sched.c", 300),
    ];
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "name".into(),
            op: CompareOp::Like,
            value: PredicateValue::String("Kconfig%".into()),
        }],
        ..Default::default()
    };
    apply_clauses(&mut items, &clauses);
    assert_eq!(items.len(), 2);
}

#[test]
fn file_entry_runtime_artifacts_detected() {
    use crate::result::FileEntry;
    for name in [
        ".git",
        ".forgeql-session",
        ".forgeql-index",
        "dir/.forgeql-columnar-delta",
    ] {
        assert!(
            FileEntry::is_runtime_artifact(std::path::Path::new(name)),
            "{name} should be a runtime artifact"
        );
    }
    for name in [
        ".gitignore",
        ".gitattributes",
        "Kconfig",
        "src/.editorconfig",
    ] {
        assert!(
            !FileEntry::is_runtime_artifact(std::path::Path::new(name)),
            "{name} must NOT be filtered"
        );
    }
}
