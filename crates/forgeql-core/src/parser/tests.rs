use super::*;
use crate::ir::ChangeTarget;

mod backends;
mod clauses;
mod find;
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

// ── UNDO ───────────────────────────────────────────────────────────────────

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
