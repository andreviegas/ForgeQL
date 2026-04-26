/// `ForgeQL` DSL parser: `.fql` text → `ForgeQLIR`.
///
/// Uses the `pest` PEG grammar defined in `forgeql.pest`.
/// Both this parser and the JSON-RPC handler produce `ForgeQLIR` —
/// there is one execution path, not two.
use pest::Parser;
use pest_derive::Parser;

use crate::error::ForgeError;
use crate::ir::ForgeQLIR;

mod change;
mod clauses;
mod find;
mod helpers;
mod transaction;

use change::{parse_change, parse_copy_or_move};
use clauses::parse_clauses;
use find::parse_find;
use helpers::{enrich_parse_error, next_str, unquote};
use transaction::parse_transaction;
/// The generated parser struct (`pest_derive` expands this at compile time).
#[derive(Parser)]
#[grammar = "parser/forgeql.pest"]
pub struct ForgeQLParser;

/// Parse a `ForgeQL` string and return each statement together with its
/// original source text.
///
/// The source text is trimmed whitespace from the pest `statement` span,
/// so multi-statement inputs like `"CMD1\nCMD2"` yield two pairs, each
/// carrying only its own command text — ready for logging.
///
/// # Errors
/// Returns `Err` if the input does not conform to the `ForgeQL` grammar, or
/// if an unhandled grammar rule is encountered during dispatch.
pub fn parse_with_source(input: &str) -> Result<Vec<(String, ForgeQLIR)>, ForgeError> {
    let pairs = ForgeQLParser::parse(Rule::program, input)
        .map_err(|e| ForgeError::DslParse(enrich_parse_error(input, e.to_string())))?;

    let mut ops = Vec::new();

    for pair in pairs {
        for statement in pair.into_inner() {
            if statement.as_rule() == Rule::EOI {
                continue;
            }
            let source_text = statement.as_str().trim().to_string();
            // Each `statement` rule has exactly one inner variant.
            let inner = statement
                .into_inner()
                .next()
                .ok_or_else(|| ForgeError::DslParse("empty statement wrapper".into()))?;
            ops.push((source_text, parse_statement(inner)?));
        }
    }

    Ok(ops)
}

/// Parse a `ForgeQL` string and return a list of operations.
///
/// Convenience wrapper around [`parse_with_source`] that discards the
/// per-statement source text.
///
/// # Errors
/// Returns `Err` if the input does not conform to the `ForgeQL` grammar, or
/// if an unhandled grammar rule is encountered during dispatch.
pub fn parse(input: &str) -> Result<Vec<ForgeQLIR>, ForgeError> {
    parse_with_source(input).map(|v| v.into_iter().map(|(_, ir)| ir).collect())
}

// parse_statement is inherently long: one match arm per grammar rule.
#[allow(clippy::too_many_lines)]
fn parse_statement(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    match pair.as_rule() {
        Rule::create_source_stmt => {
            let mut inner = pair.into_inner();
            let name = next_str(&mut inner, "create_source: expected name")?;
            let url = next_str(&mut inner, "create_source: expected url")?;
            Ok(ForgeQLIR::CreateSource { name, url })
        }

        Rule::refresh_source_stmt => {
            let mut inner = pair.into_inner();
            let name = next_str(&mut inner, "refresh_source: expected name")?;
            Ok(ForgeQLIR::RefreshSource { name })
        }

        Rule::use_stmt => {
            let mut inner = pair.into_inner();
            let source = inner
                .next()
                .map(|p| p.as_str().to_string())
                .ok_or_else(|| ForgeError::DslParse("use: expected source name".into()))?;
            let branch = inner
                .next()
                .map(|p| p.as_str().to_string())
                .ok_or_else(|| ForgeError::DslParse("use: expected branch name".into()))?;
            // Mandatory: AS 'branch-name' — enforced at grammar level, but we also
            // extract it here and treat a missing value as a hard parse error.
            let as_branch = inner.next().map(|p| unquote(p.as_str())).ok_or_else(|| {
                ForgeError::DslParse(
                    "USE requires AS 'branch-name': e.g.  USE source.branch AS 'my-feature'".into(),
                )
            })?;
            Ok(ForgeQLIR::UseSource {
                source,
                branch,
                as_branch,
            })
        }

        Rule::show_sources_stmt => Ok(ForgeQLIR::ShowSources),

        Rule::show_branches_stmt => Ok(ForgeQLIR::ShowBranches),

        Rule::show_stats_stmt => {
            let session_id = pair.into_inner().next().map(|p| unquote(p.as_str()));
            Ok(ForgeQLIR::ShowStats { session_id })
        }

        Rule::show_context_stmt => {
            let mut inner = pair.into_inner();
            let symbol = next_str(&mut inner, "show_context: expected symbol name")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowContext { symbol, clauses })
        }

        Rule::show_signature_stmt => {
            let mut inner = pair.into_inner();
            let symbol = next_str(&mut inner, "show_signature: expected symbol name")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowSignature { symbol, clauses })
        }

        Rule::show_outline_stmt => {
            let mut inner = pair.into_inner();
            let file = next_str(&mut inner, "show_outline: expected file path")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowOutline { file, clauses })
        }

        Rule::show_members_stmt => {
            let mut inner = pair.into_inner();
            let symbol = next_str(&mut inner, "show_members: expected symbol name")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowMembers { symbol, clauses })
        }

        Rule::show_body_stmt => {
            let mut inner = pair.into_inner();
            let symbol = next_str(&mut inner, "show_body: expected symbol name")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowBody { symbol, clauses })
        }

        Rule::show_callees_stmt => {
            let mut inner = pair.into_inner();
            let symbol = next_str(&mut inner, "show_callees: expected symbol name")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowCallees { symbol, clauses })
        }

        Rule::show_lines_stmt => {
            let mut inner = pair.into_inner();
            let start_line: usize = inner
                .next()
                .ok_or_else(|| ForgeError::DslParse("show_lines: expected start line".into()))?
                .as_str()
                .parse()
                .map_err(|e| ForgeError::DslParse(format!("show_lines start: {e}")))?;
            let end_line: usize = inner
                .next()
                .ok_or_else(|| ForgeError::DslParse("show_lines: expected end line".into()))?
                .as_str()
                .parse()
                .map_err(|e| ForgeError::DslParse(format!("show_lines end: {e}")))?;
            let file = next_str(&mut inner, "show_lines: expected file")?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                clauses,
            })
        }

        Rule::change_stmt => parse_change(pair),

        Rule::copy_stmt => parse_copy_or_move(pair, false),
        Rule::move_stmt => parse_copy_or_move(pair, true),

        Rule::find_stmt => parse_find(pair),

        // `statement` is a grammar wrapper — unwrap one level.
        Rule::statement => {
            let inner = pair
                .into_inner()
                .next()
                .ok_or_else(|| ForgeError::DslParse("empty wrapper rule".into()))?;
            parse_statement(inner)
        }

        Rule::transaction_stmt => parse_transaction(pair),

        Rule::rollback_stmt => {
            let name = pair.into_inner().next().map(|l| unquote(l.as_str()));
            Ok(ForgeQLIR::Rollback { name })
        }

        Rule::verify_stmt => {
            let step = pair
                .into_inner()
                .next()
                .map(|l| unquote(l.as_str()))
                .ok_or_else(|| ForgeError::DslParse("verify: expected step name".into()))?;
            Ok(ForgeQLIR::VerifyBuild { step })
        }

        Rule::commit_stmt => {
            let message = pair
                .into_inner()
                .next()
                .map(|l| unquote(l.as_str()))
                .ok_or_else(|| ForgeError::DslParse("commit: expected message".into()))?;
            Ok(ForgeQLIR::Commit { message })
        }

        r => Err(ForgeError::DslParse(format!("unhandled rule: {r:?}"))),
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ChangeTarget, CompareOp, PredicateValue, SortDirection};
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
            ForgeQLIR::FindSymbols { clauses } => {
                assert_eq!(clauses.where_predicates.len(), 1);
                let p = &clauses.where_predicates[0];
                assert_eq!(p.field, "name");
                assert_eq!(p.op, CompareOp::Like);
                assert_eq!(p.value, PredicateValue::String("set%".into()));
                assert_eq!(clauses.in_glob.as_deref(), Some("src/**/*.cpp"));
                assert!(clauses.exclude_glob.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_find_with_exclude() {
        let ops = parse("FIND symbols WHERE name LIKE 'set%' EXCLUDE 'tests/**'").unwrap();
        match &ops[0] {
            ForgeQLIR::FindSymbols { clauses } => {
                assert_eq!(clauses.where_predicates.len(), 1);
                let p = &clauses.where_predicates[0];
                assert_eq!(p.field, "name");
                assert_eq!(p.op, CompareOp::Like);
                assert_eq!(p.value, PredicateValue::String("set%".into()));
                assert!(clauses.in_glob.is_none());
                assert_eq!(clauses.exclude_glob.as_deref(), Some("tests/**"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_find_usages_with_exclude() {
        let ops = parse("FIND usages OF 'showCode' EXCLUDE 'tests/**'").unwrap();
        match &ops[0] {
            ForgeQLIR::FindUsages { of, clauses } => {
                assert_eq!(of, "showCode");
                assert_eq!(clauses.exclude_glob.as_deref(), Some("tests/**"));
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
            ForgeQLIR::ShowContext { symbol, clauses } => {
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
            ForgeQLIR::ShowContext { symbol, clauses } => {
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
            ForgeQLIR::ShowBody { symbol, clauses } => {
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
            ForgeQLIR::ShowBody { symbol, clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
        let ops =
            parse("CHANGE FILE 'file.cpp' MATCHING WORD 'declaration' WITH 'variable'").unwrap();
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
        let input =
            format!("CHANGE FILE {q}x.cpp{q} MATCHING {q}old_fn{q} WITH <<END\nnew_fn\nEND");
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
            ForgeQLIR::FindFiles { clauses } => {
                assert_eq!(clauses.in_glob.as_deref(), Some("include/**"));
                assert!(clauses.exclude_glob.is_none());
                assert!(clauses.depth.is_none());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_find_files_with_exclude() {
        let ops = parse("FIND files IN 'src/**' EXCLUDE 'src/legacy/**'").unwrap();
        match &ops[0] {
            ForgeQLIR::FindFiles { clauses } => {
                assert_eq!(clauses.in_glob.as_deref(), Some("src/**"));
                assert_eq!(clauses.exclude_glob.as_deref(), Some("src/legacy/**"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn parse_find_files_with_depth() {
        let ops = parse("FIND files IN 'src/**' DEPTH 1").unwrap();
        match &ops[0] {
            ForgeQLIR::FindFiles { clauses } => {
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
            ForgeQLIR::FindFiles { clauses } => {
                assert_eq!(clauses.in_glob.as_deref(), Some("src/**"));
                assert_eq!(clauses.exclude_glob.as_deref(), Some("src/legacy/**"));
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
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
            ForgeQLIR::FindSymbols { clauses } => {
                let p = &clauses.where_predicates[0];
                assert_eq!(p.op, CompareOp::NotMatches);
                assert_eq!(p.value, PredicateValue::String("^test_".into()));
            }
            _ => panic!("wrong variant"),
        }
    }
}
