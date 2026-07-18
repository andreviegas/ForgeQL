use super::*;
use crate::ir::{Backend, ChangeTarget, CompareOp, PredicateValue, SortDirection};
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
fn parse_transaction() {
    let fql = "BEGIN TRANSACTION 'refactor-signal'";
    let ops = parse(fql).unwrap();
    match &ops[0] {
        ForgeQLIR::BeginTransaction { name } => {
            assert_eq!(name, "refactor-signal");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_commit_standalone() {
    let fql = "COMMIT MESSAGE 'Refactor signal controller'";
    let ops = parse(fql).unwrap();
    match &ops[0] {
        ForgeQLIR::Commit { message } => {
            assert_eq!(message, "Refactor signal controller");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_checkpoint_sequence() {
    let fql = "BEGIN TRANSACTION 'refactor-signal'\n\
                   CHANGE FILE 'src/signal.cpp' MATCHING 'setPeakLevel' WITH 'setMaxIntensity'\n\
                   VERIFY build 'release'\n\
                   COMMIT MESSAGE 'Refactor signal controller'";
    let ops = parse(fql).unwrap();
    assert_eq!(ops.len(), 4);
    assert!(matches!(&ops[0], ForgeQLIR::BeginTransaction { .. }));
    assert!(matches!(&ops[1], ForgeQLIR::ChangeContent { .. }));
    assert!(matches!(&ops[2], ForgeQLIR::VerifyBuild { .. }));
    assert!(matches!(&ops[3], ForgeQLIR::Commit { .. }));
}

#[test]
fn parse_error_missing_quote() {
    let result =
        parse("CHANGE FILE 'src/foo.cpp RENAME symbol 'setPeakLevel' TO 'setMaxIntensity'");
    assert!(result.is_err());
}

#[test]
fn parse_create_source() {
    let ops = parse("CREATE SOURCE 'pisco' FROM 'git@github.com:org/pisco.git'").unwrap();
    match &ops[0] {
        ForgeQLIR::CreateSource { name, url } => {
            assert_eq!(name, "pisco");
            assert_eq!(url, "git@github.com:org/pisco.git");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_refresh_source() {
    let ops = parse("REFRESH SOURCE 'pisco-code'").unwrap();
    match &ops[0] {
        ForgeQLIR::RefreshSource { name } => {
            assert_eq!(name, "pisco-code");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_vacuum() {
    let ops = parse("VACUUM SOURCE 'pisco-code' KEEP 2 ALL APPLY").unwrap();
    match &ops[0] {
        ForgeQLIR::Vacuum {
            source,
            keep,
            all,
            apply,
        } => {
            assert_eq!(source.as_deref(), Some("pisco-code"));
            assert_eq!(*keep, 2);
            assert!(*all);
            assert!(*apply);
        }
        _ => panic!("wrong variant"),
    }
    // Bare VACUUM previews every source with conservative defaults.
    let ops = parse("VACUUM").unwrap();
    match &ops[0] {
        ForgeQLIR::Vacuum {
            source,
            keep,
            all,
            apply,
        } => {
            assert_eq!(*source, None);
            assert_eq!(*keep, 0);
            assert!(!*all);
            assert!(!*apply);
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_use_source_without_as_is_error() {
    // USE without AS 'branch-name' must be a parse error (grammar enforces it)
    assert!(
        parse("USE pisco.main").is_err(),
        "USE without AS should be a parse error"
    );
    assert!(
        parse("USE pisco-code.main").is_err(),
        "USE without AS (hyphenated source) should be a parse error"
    );
}

#[test]
fn parse_use_source_with_as() {
    // plain identifier
    let ops = parse("USE pisco.main AS 'my-feature'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "pisco");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "my-feature");
        }
        _ => panic!("wrong variant"),
    }
    // hyphenated source name
    let ops2 = parse("USE pisco-code.main AS 'refactor'").unwrap();
    match &ops2[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "pisco-code");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "refactor");
        }
        _ => panic!("wrong variant for hyphenated name"),
    }
}

#[test]
fn parse_use_source_commit_hash_branch() {
    // Digit-led commit hash — unparseable before commit-base support was added.
    let ops = parse("USE forgeql-pub.594cc8b AS 'review'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "forgeql-pub");
            assert_eq!(branch, "594cc8b");
            assert_eq!(as_branch, "review");
        }
        _ => panic!("wrong variant for commit-hash branch"),
    }
    // A full 40-char hash parses too.
    let ops2 = parse("USE forgeql-pub.0c7a0fb14ea282d260b1b2af035c8c55a174e437 AS 'r2'").unwrap();
    match &ops2[0] {
        ForgeQLIR::UseSource { branch, .. } => {
            assert_eq!(branch, "0c7a0fb14ea282d260b1b2af035c8c55a174e437");
        }
        _ => panic!("wrong variant for full-hash branch"),
    }
}

#[test]
fn parse_show_sources() {
    let ops = parse("SHOW SOURCES").unwrap();
    assert!(matches!(ops[0], ForgeQLIR::ShowSources));
}

#[test]
fn parse_show_branches_with_source_is_rejected() {
    let q = char::from(39u8);
    let input = format!("SHOW BRANCHES OF {q}pisco{q}");
    assert!(parse(&input).is_err());
}

#[test]
fn parse_show_branches() {
    let ops = parse("SHOW BRANCHES").unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], ForgeQLIR::ShowBranches));
}

#[test]
fn parse_show_version() {
    let ops = parse("SHOW VERSION").unwrap();
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], ForgeQLIR::ShowVersion));
}

#[test]
fn parse_show_stats_no_session() {
    let ops = parse("SHOW STATS").unwrap();
    assert!(matches!(ops[0], ForgeQLIR::ShowStats { session_id: None }));
}

#[test]
fn parse_show_stats_for_session() {
    let ops = parse("SHOW STATS FOR 'my-session'").unwrap();
    assert!(matches!(
        ops[0],
        ForgeQLIR::ShowStats { session_id: Some(ref s) } if s == "my-session"
    ));
}

// (parse_disconnect test removed — DISCONNECT command eliminated)

// -----------------------------------------------------------------------
// SHOW commands (Code Exposure API)
// -----------------------------------------------------------------------

#[test]
fn parse_show_context_minimal() {
    let ops = parse("SHOW context OF 'setPeakLevel'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowContext {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "setPeakLevel");
            assert!(clauses.in_glob.is_none());
            assert!(clauses.depth.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_context_with_file_and_lines() {
    // IN 'file' → clauses.in_glob; LINES 10 → clauses.depth
    let ops = parse("SHOW context OF 'setPeakLevel' IN 'src/signal.cpp' LINES 10").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowContext {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "setPeakLevel");
            assert_eq!(clauses.in_glob.as_deref(), Some("src/signal.cpp"));
            assert_eq!(clauses.depth, Some(10));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_signature() {
    let ops = parse("SHOW signature OF 'setPeakLevel'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowSignature { symbol, .. } => assert_eq!(symbol, "setPeakLevel"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_outline() {
    let ops = parse("SHOW outline OF 'src/signal.cpp'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowOutline { file, .. } => assert_eq!(file, "src/signal.cpp"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_members() {
    let ops = parse("SHOW members OF 'SignalController'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMembers { symbol, .. } => assert_eq!(symbol, "SignalController"),
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_body_no_depth() {
    let ops = parse("SHOW body OF 'processSignal'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "processSignal");
            assert!(clauses.depth.is_none());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_body_with_depth() {
    let ops = parse("SHOW body OF 'processSignal' DEPTH 2").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody {
            symbol, clauses, ..
        } => {
            assert_eq!(symbol, "processSignal");
            assert_eq!(clauses.depth, Some(2));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_callees() {
    let ops = parse("SHOW callees OF 'processSignal'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowCallees { symbol, .. } => assert_eq!(symbol, "processSignal"),
        _ => panic!("wrong variant"),
    }
}

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

// ── SHOW LINES / FIND files ──────────────────────────────────────────────

#[test]
fn parse_show_more_bare_is_full() {
    let ops = parse("SHOW MORE").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMore {
            window, clauses, ..
        } => {
            assert_eq!(*window, crate::ir::ShowMoreWindow::Full);
            assert!(clauses.where_predicates.is_empty());
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_head_tail_range() {
    use crate::ir::ShowMoreWindow;
    let head = parse("SHOW MORE HEAD 20").unwrap();
    let tail = parse("SHOW MORE TAIL 15").unwrap();
    let range = parse("SHOW MORE 120-240").unwrap();
    match &head[0] {
        ForgeQLIR::ShowMore { window, .. } => assert_eq!(*window, ShowMoreWindow::Head(20)),
        _ => panic!("wrong variant"),
    }
    match &tail[0] {
        ForgeQLIR::ShowMore { window, .. } => assert_eq!(*window, ShowMoreWindow::Tail(15)),
        _ => panic!("wrong variant"),
    }
    match &range[0] {
        ForgeQLIR::ShowMore { window, .. } => {
            assert_eq!(*window, ShowMoreWindow::Range(120, 240));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_last_n() {
    use crate::ir::ShowMoreWindow;
    // Bare SHOW MORE defaults to LAST-0.
    match &parse("SHOW MORE").unwrap()[0] {
        ForgeQLIR::ShowMore { last, window, .. } => {
            assert_eq!(*last, 0);
            assert_eq!(*window, ShowMoreWindow::Full);
        }
        _ => panic!("wrong variant"),
    }
    // LAST-n selector alone.
    match &parse("SHOW MORE LAST-2").unwrap()[0] {
        ForgeQLIR::ShowMore { last, window, .. } => {
            assert_eq!(*last, 2);
            assert_eq!(*window, ShowMoreWindow::Full);
        }
        _ => panic!("wrong variant"),
    }
    // LAST-n composes with a range window — the atomic LAST-<n> token never
    // collides with the range hyphen.
    match &parse("SHOW MORE LAST-1 1-1000").unwrap()[0] {
        ForgeQLIR::ShowMore { last, window, .. } => {
            assert_eq!(*last, 1);
            assert_eq!(*window, ShowMoreWindow::Range(1, 1000));
        }
        _ => panic!("wrong variant"),
    }
}

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
fn parse_job_commands() {
    match &parse("JOB START 'test-all'").unwrap()[0] {
        ForgeQLIR::JobStart { label, args } => {
            assert_eq!(label, "test-all");
            assert!(args.is_empty());
        }
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("JOB START 'bless' 'zephyr' 'pytorch'").unwrap()[0] {
        ForgeQLIR::JobStart { label, args } => {
            assert_eq!(label, "bless");
            assert_eq!(args, &["zephyr".to_string(), "pytorch".to_string()]);
        }
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("JOB STATUS 'j-00001a'").unwrap()[0] {
        ForgeQLIR::JobStatus { id } => assert_eq!(id, "j-00001a"),
        other => panic!("wrong variant: {other:?}"),
    }
    match &parse("JOB LIST").unwrap()[0] {
        ForgeQLIR::JobList => {}
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn parse_show_more_window_then_where_composes() {
    // WHERE must apply after every window form, not just bare SHOW MORE.
    let ops = parse("SHOW MORE TAIL 40 WHERE text MATCHES 'error|fail'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMore {
            window, clauses, ..
        } => {
            assert_eq!(*window, crate::ir::ShowMoreWindow::Tail(40));
            assert_eq!(clauses.where_predicates.len(), 1);
            assert_eq!(clauses.where_predicates[0].field, "text");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_more_range_then_where_and_limit() {
    let ops = parse("SHOW MORE 1-400 WHERE text LIKE '%warning%' LIMIT 10").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowMore {
            window, clauses, ..
        } => {
            assert_eq!(*window, crate::ir::ShowMoreWindow::Range(1, 400));
            assert_eq!(clauses.where_predicates.len(), 1);
            assert_eq!(clauses.limit, Some(10));
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_show_lines() {
    let ops = parse("SHOW LINES 10-25 OF 'src/signal_controller.cpp'").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowLines {
            file,
            start_line,
            end_line,
            ..
        } => {
            assert_eq!(file, "src/signal_controller.cpp");
            assert_eq!(*start_line, 10);
            assert_eq!(*end_line, 25);
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

#[test]
fn parse_use_source_without_as_is_a_parse_error() {
    // This test replaces parse_use_source_without_as_has_no_as_branch.
    // AS 'branch-name' is now mandatory — omitting it must be a parse error.
    let err = parse("USE pisco-code.main");
    assert!(err.is_err(), "USE without AS should be a parse error");
    let msg = format!("{}", err.unwrap_err());
    assert!(
        msg.contains("USE requires an AS clause"),
        "error should hint about AS clause; got: {msg}"
    );
}

#[test]
fn parse_use_source_as_sets_as_branch() {
    let ops = parse("USE pisco-code.main AS 'agent/refactor-signal-api'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "pisco-code");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "agent/refactor-signal-api");
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

// ── Relaxed quoting (double-quoted and bare values) ──────────────────────

#[test]
fn parse_use_source_hyphenated_branch() {
    // Branch position now uses source_name instead of identifier → hyphens accepted.
    let ops = parse("USE forgeql-pub.line-budget AS 'lb2'").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "forgeql-pub");
            assert_eq!(branch, "line-budget");
            assert_eq!(as_branch, "lb2");
        }
        _ => panic!("wrong variant"),
    }
}
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

#[test]
fn parse_show_body_bare_name() {
    // SHOW body OF symbol_name (no quotes).
    let ops = parse("SHOW body OF sweep_expired").unwrap();
    match &ops[0] {
        ForgeQLIR::ShowBody { symbol, .. } => {
            assert_eq!(symbol, "sweep_expired");
        }
        _ => panic!("wrong variant"),
    }
}

#[test]
fn parse_use_source_bare_as_branch() {
    // AS target accepts bare (unquoted) value.
    let ops = parse("USE forgeql-pub.main AS my-feature").unwrap();
    match &ops[0] {
        ForgeQLIR::UseSource {
            source,
            branch,
            as_branch,
        } => {
            assert_eq!(source, "forgeql-pub");
            assert_eq!(branch, "main");
            assert_eq!(as_branch, "my-feature");
        }
        _ => panic!("wrong variant"),
    }
}
// -- missing error path tests -----------------------------------------

#[test]
fn parse_use_missing_dot_is_error() {
    // USE requires "source.branch" with a dot separator.
    assert!(
        parse("USE pisco main AS 'my-alias'").is_err(),
        "USE without dot separator should be rejected"
    );
}

#[test]
fn parse_find_unknown_target_is_error() {
    assert!(
        parse("FIND everything").is_err(),
        "FIND with unknown target should be a parse error"
    );
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
