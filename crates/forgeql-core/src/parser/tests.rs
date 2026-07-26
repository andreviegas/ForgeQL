use super::*;
use crate::ir::{ChangeTarget, CompareOp, PredicateValue, SortDirection};

mod backends;
mod clauses;
mod show;
mod sources;
mod transactions;

#[test]
fn parse_top_level_rename_is_rejected() {
    // Top-level RENAME was removed in v0.10.0.
    let result = parse("RENAME symbol 'setPeakLevel' TO 'setMaxIntensity'");
    assert!(result.is_err(), "top-level RENAME should be a parse error");
}

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
fn parse_error_missing_quote() {
    let result =
        parse("CHANGE FILE 'src/foo.cpp RENAME symbol 'setPeakLevel' TO 'setMaxIntensity'");
    assert!(result.is_err());
}

// ── CHANGE command ───────────────────────────────────────────────────────

#[test]
fn parse_change_with_content() {
    let ops = parse("CHANGE FILE 'src/new.cpp' WITH 'int main() {}'").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["src/new.cpp"]);
            assert!(
                matches!(target, ChangeTarget::WithContent { content } if content == "int main() {}")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_change_matching() {
    let ops = parse("CHANGE FILE 'file.cpp' MATCHING '#define BAUD 9600' WITH 'constexpr uint32_t BAUD = 9600;'").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["file.cpp"]);
            assert!(
                matches!(target, ChangeTarget::Matching { pattern, replacement, .. }
                    if pattern == "#define BAUD 9600" && replacement == "constexpr uint32_t BAUD = 9600;")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_change_matching_word() {
    let ops = parse("CHANGE FILE 'file.cpp' MATCHING WORD 'declaration' WITH 'variable'").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { target, .. } => match target {
            ChangeTarget::Matching {
                pattern,
                replacement,
                word_boundary,
            } => {
                assert_eq!(pattern, "declaration");
                assert_eq!(replacement, "variable");
                assert!(word_boundary, "WORD modifier should set word_boundary=true");
            }
            other => panic!("expected Matching, got {other:?}"),
        },
        _ => panic!("wrong variant"),
    }
}
#[test]
fn parse_change_lines() {
    let ops = parse("CHANGE FILE 'file.cpp' LINES 10-15 WITH 'new code'").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["file.cpp"]);
            assert!(
                matches!(target, ChangeTarget::Lines { start: 10, end: 15, content } if content == "new code")
            );
        }
        _ => panic!("wrong variant"),
    }
}

// ── Heredoc WITH tests ─────────────────────────────────────────────────────

#[test]
fn parse_change_with_heredoc_basic() {
    let q = char::from(39u8);
    let input = format!("CHANGE FILE {q}src/lib.rs{q} WITH <<RUST\nfn hello() {{}}\nRUST");
    let ops = parse(&input).unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["src/lib.rs"]);
            assert!(
                matches!(target, ChangeTarget::WithContent { content } if content == "fn hello() {}")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_change_lines_heredoc() {
    let q = char::from(39u8);
    let input = format!("CHANGE FILE {q}src/lib.rs{q} LINES 5-10 WITH <<CODE\nreturn 0;\nCODE");
    let ops = parse(&input).unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["src/lib.rs"]);
            assert!(
                matches!(target, ChangeTarget::Lines { start: 5, end: 10, content } if content == "return 0;")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_change_matching_heredoc() {
    let q = char::from(39u8);
    let input = format!("CHANGE FILE {q}x.cpp{q} MATCHING {q}old_fn{q} WITH <<END\nnew_fn\nEND");
    let ops = parse(&input).unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["x.cpp"]);
            assert!(
                matches!(target, ChangeTarget::Matching { pattern, replacement, .. }
                    if pattern == "old_fn" && replacement == "new_fn")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_heredoc_multiline() {
    let q = char::from(39u8);
    let input = format!("CHANGE FILE {q}a.rs{q} WITH <<BLOCK\nline one\nline two\nBLOCK");
    let ops = parse(&input).unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { target, .. } => {
            assert!(
                matches!(target, ChangeTarget::WithContent { content } if content == "line one\nline two")
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_heredoc_body_with_single_quotes() {
    // The main motivation: single quotes inside heredoc body must not break parsing
    let q = char::from(39u8);
    let expected = format!("let c = {q}x{q};");
    let input = format!("CHANGE FILE {q}a.rs{q} WITH <<RUST\nlet c = {q}x{q};\nRUST");
    let ops = parse(&input).unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { target, .. } => {
            assert!(
                matches!(target, ChangeTarget::WithContent { content } if content == &expected)
            );
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_heredoc_mismatched_tags_is_error() {
    let q = char::from(39u8);
    let input = format!("CHANGE FILE {q}a.rs{q} WITH <<OPEN\ncontent\nCLOSE");
    assert!(parse(&input).is_err());
}

#[test]
fn parse_heredoc_lowercase_tag_is_rejected() {
    let q = char::from(39u8);
    let input = format!("CHANGE FILE {q}a.rs{q} WITH <<rust\ncontent\nrust");
    assert!(parse(&input).is_err());
}

#[test]
fn parse_commit_message_heredoc_with_apostrophes() {
    // COMMIT MESSAGE now accepts a heredoc, so a message may contain single
    // quotes / apostrophes that would otherwise terminate the quoted form.
    let input = "COMMIT MESSAGE <<MSG\nfix: don't drop the agent's apostrophes\nMSG";
    let ops = parse(input).unwrap();
    match &ops[0] {
        ForgeQLIR::Commit { message } => {
            assert_eq!(message, "fix: don't drop the agent's apostrophes");
        }
        _ => panic!("wrong variant"),
    }
}
#[test]
fn parse_change_delete() {
    let ops = parse("CHANGE FILES 'a.cpp', 'b.h' WITH NOTHING").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["a.cpp", "b.h"]);
            assert!(matches!(target, ChangeTarget::Delete));
        }
        _ => panic!("wrong variant"),
    }
}

// ── BUG #2 regression: CHANGE FILE 'f' LINES n-m NOTHING must parse ──────

#[test]
fn parse_change_lines_nothing() {
    let ops = parse("CHANGE FILE 'src/test.cpp' LINES 1-3 NOTHING").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["src/test.cpp"]);
            // Must produce Lines with empty content (deletion).
            assert!(
                matches!(target, ChangeTarget::Lines { start: 1, end: 3, content } if content.is_empty()),
                "expected Lines{{1,3,\"\"}} got {target:?}"
            );
        }
        other => panic!("expected ChangeContent, got {other:?}"),
    }
}

#[test]
fn parse_change_lines_with_nothing() {
    let ops = parse("CHANGE FILE 'src/test.cpp' LINES 1-3 WITH NOTHING").unwrap();
    match &ops[0] {
        ForgeQLIR::ChangeContent { files, target, .. } => {
            assert_eq!(files, &["src/test.cpp"]);
            assert!(
                matches!(target, ChangeTarget::Lines { start: 1, end: 3, content } if content.is_empty()),
                "expected Lines{{1,3,\"\"}} got {target:?}"
            );
        }
        other => panic!("expected ChangeContent, got {other:?}"),
    }
}
#[test]
fn parse_change_in_transaction_sequence() {
    let fql = "BEGIN TRANSACTION 'test-change'\n\
                   CHANGE FILE 'file.cpp' MATCHING 'old' WITH 'new'\n\
                   COMMIT MESSAGE 'test'";
    let ops = parse(fql).unwrap();
    assert_eq!(ops.len(), 3);
    assert!(matches!(&ops[0], ForgeQLIR::BeginTransaction { .. }));
    assert!(matches!(&ops[1], ForgeQLIR::ChangeContent { .. }));
    assert!(matches!(&ops[2], ForgeQLIR::Commit { .. }));
}

// ── UNDO / FIND files ──────────────────────────────────────────────────────

#[test]
fn parse_undo_last_n() {
    // Bare UNDO defaults to LAST-0.
    match &parse("UNDO").unwrap()[0] {
        ForgeQLIR::Undo { last } => assert_eq!(*last, 0),
        _ => panic!("wrong variant"),
    }
    // LAST-n selector reuses the atomic LAST-<n> token.
    match &parse("UNDO LAST-3").unwrap()[0] {
        ForgeQLIR::Undo { last } => assert_eq!(*last, 3),
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
