/// `ForgeQL` DSL parser: `.fql` text → `ForgeQLIR`.
///
/// Uses the `pest` PEG grammar defined in `forgeql.pest`.
/// Both this parser and the JSON-RPC handler produce `ForgeQLIR` —
/// there is one execution path, not two.
use pest::Parser;
use pest_derive::Parser;

use crate::error::ForgeError;
use crate::ir::{
    ChangeTarget, Clauses, CompareOp, ForgeQLIR, GroupBy, OrderBy, Predicate, PredicateValue,
    SortDirection,
};

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

/// Advance `pairs` and return the next item as an unquoted `String`.
fn next_str(
    pairs: &mut pest::iterators::Pairs<'_, Rule>,
    msg: &'static str,
) -> Result<String, ForgeError> {
    pairs
        .next()
        .map(|p| unquote(p.as_str()))
        .ok_or_else(|| ForgeError::DslParse(msg.into()))
}

/// Parse the universal `clauses` block.
///
/// Accepts the inner pairs of a `clauses` rule (or any iterator of clause
/// pairs) and fills a `Clauses` struct.  Unknown rule types are silently
/// skipped so that forward-compatible clause additions don't break old code.
fn parse_clauses(pairs: pest::iterators::Pairs<'_, Rule>) -> Clauses {
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
fn parse_predicate(pair: pest::iterators::Pair<'_, Rule>) -> Option<Predicate> {
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
fn parse_compare_op(op_str: &str) -> CompareOp {
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

fn parse_find(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();
    let target_pair = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("find: expected target".into()))?;

    // Collect remaining pairs; the only one should be the `clauses` node.
    let clauses = parse_clauses(inner);

    let target_str = target_pair.as_str().trim();

    // "usages OF 'name'" — dedicated variant
    if target_str.starts_with("usages") {
        let name = target_pair
            .into_inner()
            .next()
            .map(|p| unquote(p.as_str()))
            .unwrap_or_default();
        return Ok(ForgeQLIR::FindUsages { of: name, clauses });
    }

    // "callees OF 'func'" — routes to ShowCallees (calls graph query)
    if target_str.starts_with("callees") {
        let symbol = target_pair
            .into_inner()
            .next()
            .map(|p| unquote(p.as_str()))
            .unwrap_or_default();
        return Ok(ForgeQLIR::ShowCallees { symbol, clauses });
    }

    match target_str {
        "globals" => {
            // Convenience alias: FIND globals →
            //   FIND symbols WHERE node_kind = 'declaration' WHERE scope = 'file'
            let kind_pred = Predicate {
                field: "node_kind".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("declaration".into()),
            };
            let scope_pred = Predicate {
                field: "scope".into(),
                op: CompareOp::Eq,
                value: PredicateValue::String("file".into()),
            };
            let mut clauses = clauses;
            clauses.where_predicates.push(kind_pred);
            clauses.where_predicates.push(scope_pred);
            Ok(ForgeQLIR::FindSymbols { clauses })
        }
        "files" => Ok(ForgeQLIR::FindFiles { clauses }),
        _ => Ok(ForgeQLIR::FindSymbols { clauses }),
    }
}

fn parse_change(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();

    // file_list → one or more string_literal children
    let file_list_pair = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("change: expected file_list".into()))?;
    let files: Vec<String> = file_list_pair
        .into_inner()
        .map(|p| unquote(p.as_str()))
        .collect();
    if files.is_empty() {
        return Err(ForgeError::DslParse("change: file_list is empty".into()));
    }

    // change_target → exactly one of the sub-rules
    let target_pair = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("change: expected change_target".into()))?;
    let target_inner = target_pair
        .into_inner()
        .next()
        .ok_or_else(|| ForgeError::DslParse("change: empty change_target".into()))?;

    let target = match target_inner.as_rule() {
        Rule::change_matching => {
            let mut m = target_inner.into_inner();
            let pattern = next_str(&mut m, "change_matching: expected pattern")?;
            let replacement = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_matching: expected replacement".into()))
                .and_then(unwrap_content)?;
            ChangeTarget::Matching {
                pattern,
                replacement,
            }
        }
        Rule::change_lines_delete => {
            let mut m = target_inner.into_inner();
            let start: usize = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_lines_delete: expected start".into()))?
                .as_str()
                .parse()
                .map_err(|e| ForgeError::DslParse(format!("change_lines_delete start: {e}")))?;
            let end: usize = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_lines_delete: expected end".into()))?
                .as_str()
                .parse()
                .map_err(|e| ForgeError::DslParse(format!("change_lines_delete end: {e}")))?;
            // Empty content replaces the line range with nothing (deletion).
            ChangeTarget::Lines {
                start,
                end,
                content: String::new(),
            }
        }
        Rule::change_lines_range => {
            let mut m = target_inner.into_inner();
            let start: usize = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_lines: expected start".into()))?
                .as_str()
                .parse()
                .map_err(|e| ForgeError::DslParse(format!("change_lines start: {e}")))?;
            let end: usize = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_lines: expected end".into()))?
                .as_str()
                .parse()
                .map_err(|e| ForgeError::DslParse(format!("change_lines end: {e}")))?;
            let content = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_lines: expected content".into()))
                .and_then(unwrap_content)?;
            ChangeTarget::Lines {
                start,
                end,
                content,
            }
        }
        Rule::change_delete => ChangeTarget::Delete,
        Rule::change_with_content => {
            let content = target_inner
                .into_inner()
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_with: expected content".into()))
                .and_then(unwrap_content)?;
            ChangeTarget::WithContent { content }
        }
        r => {
            return Err(ForgeError::DslParse(format!(
                "change: unhandled target rule {r:?}"
            )));
        }
    };

    // Remaining pairs: the trailing `clauses` block.
    let clauses = parse_clauses(inner);

    Ok(ForgeQLIR::ChangeContent {
        files,
        target,
        clauses,
    })
}

/// Parse `COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]` and
/// `MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]`.
///
/// `is_move` distinguishes the two variants.
fn parse_copy_or_move(
    pair: pest::iterators::Pair<'_, Rule>,
    is_move: bool,
) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();

    let start: usize = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("copy/move: expected start line".into()))?
        .as_str()
        .parse()
        .map_err(|e| ForgeError::DslParse(format!("copy/move start: {e}")))?;

    let end: usize = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("copy/move: expected end line".into()))?
        .as_str()
        .parse()
        .map_err(|e| ForgeError::DslParse(format!("copy/move end: {e}")))?;

    let src = next_str(&mut inner, "copy/move: expected source path")?;
    let dst = next_str(&mut inner, "copy/move: expected destination path")?;

    // Optional `AT LINE k`
    let at: Option<usize> = inner
        .next()
        .map(|p| {
            p.as_str()
                .parse::<usize>()
                .map_err(|e| ForgeError::DslParse(format!("copy/move AT LINE: {e}")))
        })
        .transpose()?;

    if is_move {
        Ok(ForgeQLIR::MoveLines {
            src,
            start,
            end,
            dst,
            at,
        })
    } else {
        Ok(ForgeQLIR::CopyLines {
            src,
            start,
            end,
            dst,
            at,
        })
    }
}
fn parse_transaction(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();
    let name = next_str(&mut inner, "transaction: expected name")?;
    Ok(ForgeQLIR::BeginTransaction { name })
}

/// Strip the surrounding single-quotes from a `string_literal` token.
fn unquote(s: &str) -> String {
    s.trim_matches(|c: char| c == '\'' || c == '"').to_string()
}

/// Extract the string content from a `content_value` pair, handling both
/// single-quoted string literals and heredoc blocks.
fn unwrap_content(pair: pest::iterators::Pair<Rule>) -> Result<String, ForgeError> {
    let inner = match pair.as_rule() {
        Rule::content_value => pair
            .into_inner()
            .next()
            .ok_or_else(|| ForgeError::DslParse("content_value: empty".into()))?,
        Rule::string_literal | Rule::heredoc_literal => pair,
        r => {
            return Err(ForgeError::DslParse(format!(
                "unwrap_content: unexpected {r:?}"
            )));
        }
    };
    match inner.as_rule() {
        Rule::string_literal => Ok(unquote(inner.as_str())),
        Rule::heredoc_literal => extract_heredoc(inner),
        r => Err(ForgeError::DslParse(format!(
            "unwrap_content: unexpected inner {r:?}"
        ))),
    }
}

/// Extract body text from a `heredoc_literal` pair.
/// Validates that the opening and closing tags match.
/// The body is returned without the trailing newline that precedes the closing tag.
fn extract_heredoc(pair: pest::iterators::Pair<Rule>) -> Result<String, ForgeError> {
    let mut inner = pair.into_inner();
    let open_tag = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("heredoc: missing open tag".into()))?
        .as_str();
    let body = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("heredoc: missing body".into()))?
        .as_str();
    let close_tag = inner
        .next()
        .ok_or_else(|| ForgeError::DslParse("heredoc: missing close tag".into()))?
        .as_str();
    if open_tag != close_tag {
        return Err(ForgeError::DslParse(format!(
            "heredoc: opening tag {open_tag} does not match closing tag {close_tag}"
        )));
    }
    Ok(body.to_string())
}
// -----------------------------------------------------------------------
// Error enrichment
// -----------------------------------------------------------------------

/// Detect common SQL-isms that don't exist in `ForgeQL` and append a helpful
/// hint to the pest parse error.
fn enrich_parse_error(input: &str, mut msg: String) -> String {
    let upper = input.to_uppercase();
    if upper.contains(" AND ") {
        msg.push_str(
            "\n\nHint: ForgeQL does not support the AND keyword. \
             Use multiple WHERE clauses instead.\n\
             Example: WHERE node_kind = 'function_definition' WHERE is_static = 'true'",
        );
    }
    if upper.contains(" OR ") {
        msg.push_str(
            "\n\nHint: ForgeQL does not support the OR keyword. \
             Run separate queries for each condition, or use LIKE wildcards \
             when matching alternative string patterns.\n\
             Example: WHERE name LIKE '%read%' (matches any name containing \"read\")",
        );
    }
    msg
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

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
                    matches!(target, ChangeTarget::Matching { pattern, replacement }
                    if pattern == "#define BAUD 9600" && replacement == "constexpr uint32_t BAUD = 9600;")
                );
            }
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
                    matches!(target, ChangeTarget::Matching { pattern, replacement }
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
                    .find(|p| p.field == "node_kind");
                assert!(
                    kind_pred.is_some(),
                    "globals should add a node_kind predicate"
                );
                let kp = kind_pred.unwrap();
                assert_eq!(kp.op, CompareOp::Eq);
                assert_eq!(kp.value, PredicateValue::String("declaration".into()));

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
                    .find(|p| p.field == "node_kind");
                assert!(
                    kind_pred.is_some(),
                    "globals should add a node_kind predicate"
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
                    .find(|p| p.field == "node_kind");
                assert!(
                    kind_pred.is_some(),
                    "globals should add a node_kind predicate"
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
        assert!(
            parse("USE pisco-code.main").is_err(),
            "USE without AS should be a parse error"
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
}
