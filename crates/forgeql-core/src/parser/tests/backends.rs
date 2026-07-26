//! Parser tests for the `USING` clause — backend routing.

use crate::ir::Backend;
use crate::parser::*;

// ── USING clause (Backend routing) ──────────────────────────────────────

#[test]
fn parse_find_symbols_no_using_defaults_to_default_backend() {
    let ops = parse("FIND symbols WHERE name LIKE 'fn%'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols {
            backend, clauses, ..
        } => {
            assert_eq!(*backend, Backend::Default);
            assert_eq!(clauses.where_predicates.len(), 1);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_using_legacy() {
    let ops = parse("FIND symbols USING 'legacy' WHERE name LIKE 'fn%'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { backend, .. } => {
            assert_eq!(*backend, Backend::Legacy);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_symbols_using_columnar() {
    let ops = parse("FIND symbols USING 'columnar'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindSymbols { backend, .. } => {
            assert_eq!(*backend, Backend::Columnar);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_find_usages_using_legacy() {
    let ops = parse("FIND usages OF 'myFn' USING 'legacy'").unwrap();
    match &ops[0] {
        ForgeQLIR::FindUsages { backend, of, .. } => {
            assert_eq!(*backend, Backend::Legacy);
            assert_eq!(of, "myFn");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_body_using_legacy() {
    let ops = parse("SHOW body OF 'myFn' USING 'legacy'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody {
            backend, symbol, ..
        } => {
            assert_eq!(*backend, Backend::Legacy);
            assert_eq!(symbol, "myFn");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_lines_using_columnar() {
    let ops = parse("SHOW LINES 1-10 OF 'src/lib.rs' USING 'columnar'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowLines {
            backend,
            file,
            start_line,
            end_line,
            ..
        } => {
            assert_eq!(*backend, Backend::Columnar);
            assert_eq!(file, "src/lib.rs");
            assert_eq!(*start_line, 1);
            assert_eq!(*end_line, 10);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_using_unknown_backend_is_error() {
    let result = parse("FIND symbols USING 'mysql'");
    assert!(
        result.is_err(),
        "USING with unknown backend name should be a parse error"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("mysql"),
        "error message should mention the unknown backend name: {msg}"
    );
}

#[test]
fn change_file_has_no_using_clause() {
    // Grammar does not allow USING on mutations — verify it is rejected.
    let result = parse("CHANGE FILE 'f.rs' USING 'legacy' MATCHING 'x' WITH 'y'");
    assert!(
        result.is_err(),
        "USING on a CHANGE statement should be a parse error"
    );
}
