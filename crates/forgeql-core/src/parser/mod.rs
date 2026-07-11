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
use helpers::{
    enrich_parse_error, next_str, parse_using_clause, unquote, unwrap_any_value, unwrap_content,
};
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

/// Parse a `show_more_window` rule into a [`crate::ir::ShowMoreWindow`].
fn parse_show_more_window(
    pair: pest::iterators::Pair<'_, Rule>,
) -> Result<crate::ir::ShowMoreWindow, ForgeError> {
    use crate::ir::ShowMoreWindow;
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| ForgeError::DslParse("show_more: empty window".into()))?;
    let num = |p: &pest::iterators::Pair<'_, Rule>| -> Result<usize, ForgeError> {
        p.as_str()
            .parse()
            .map_err(|e| ForgeError::DslParse(format!("show_more window number: {e}")))
    };
    let next = |i: &mut pest::iterators::Pairs<'_, Rule>,
                msg: &'static str|
     -> Result<usize, ForgeError> {
        let p = i.next().ok_or_else(|| ForgeError::DslParse(msg.into()))?;
        num(&p)
    };
    match inner.as_rule() {
        Rule::show_more_head => {
            let mut i = inner.into_inner();
            Ok(ShowMoreWindow::Head(next(
                &mut i,
                "show_more HEAD: expected number",
            )?))
        }
        Rule::show_more_tail => {
            let mut i = inner.into_inner();
            Ok(ShowMoreWindow::Tail(next(
                &mut i,
                "show_more TAIL: expected number",
            )?))
        }
        Rule::show_more_range => {
            let mut i = inner.into_inner();
            let a = next(&mut i, "show_more range: expected start")?;
            let b = next(&mut i, "show_more range: expected end")?;
            Ok(ShowMoreWindow::Range(a, b))
        }
        r => Err(ForgeError::DslParse(format!(
            "show_more: unexpected window rule {r:?}"
        ))),
    }
}

/// Parse a `show_more_stmt` rule into a [`ForgeQLIR::ShowMore`].
fn parse_show_more_stmt(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut window = crate::ir::ShowMoreWindow::Full;
    let mut clauses = crate::ir::Clauses::default();
    let mut last = 0_usize;
    for child in pair.into_inner() {
        match child.as_rule() {
            Rule::show_more_last => {
                // Atomic token "LAST-<n>": strip the prefix and parse the index.
                last = child
                    .as_str()
                    .strip_prefix("LAST-")
                    .and_then(|n| n.parse().ok())
                    .ok_or_else(|| {
                        ForgeError::DslParse("show_more: invalid LAST-n selector".into())
                    })?;
            }
            Rule::show_more_window => {
                window = parse_show_more_window(child)?;
            }
            Rule::clauses => {
                clauses = parse_clauses(child.into_inner());
            }
            _ => {}
        }
    }
    Ok(ForgeQLIR::ShowMore {
        window,
        last,
        clauses,
    })
}

/// Parse an `undo_stmt` rule into a [`ForgeQLIR::Undo`].
fn parse_undo_stmt(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut last = 0_usize;
    for child in pair.into_inner() {
        if child.as_rule() == Rule::undo_last {
            // Atomic token "LAST-<n>": strip the prefix and parse the index.
            last = child
                .as_str()
                .strip_prefix("LAST-")
                .and_then(|n| n.parse().ok())
                .ok_or_else(|| ForgeError::DslParse("undo: invalid LAST-n selector".into()))?;
        }
    }
    Ok(ForgeQLIR::Undo { last })
}

/// Parse an `export_patch_stmt` rule into a [`ForgeQLIR::ExportPatch`].
fn parse_export_patch_stmt(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut last = None;
    for child in pair.into_inner() {
        if child.as_rule() == Rule::export_last {
            // `LAST <number>` — the inner number token is the commit count.
            last = child
                .into_inner()
                .next()
                .and_then(|n| n.as_str().parse().ok())
                .map(Some)
                .ok_or_else(|| ForgeError::DslParse("export patch: invalid LAST count".into()))?;
        }
    }
    Ok(ForgeQLIR::ExportPatch { last })
}

fn parse_job_stmt(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let inner = pair
        .into_inner()
        .next()
        .ok_or_else(|| ForgeError::DslParse("job: expected START | STATUS | LIST".into()))?;
    match inner.as_rule() {
        Rule::job_start => {
            let label = inner
                .into_inner()
                .next()
                .map(|l| unquote(l.as_str()))
                .ok_or_else(|| ForgeError::DslParse("job start: expected step label".into()))?;
            Ok(ForgeQLIR::JobStart { label })
        }
        Rule::job_status => {
            let id = inner
                .into_inner()
                .next()
                .map(|l| unquote(l.as_str()))
                .ok_or_else(|| ForgeError::DslParse("job status: expected job id".into()))?;
            Ok(ForgeQLIR::JobStatus { id })
        }
        Rule::job_list => Ok(ForgeQLIR::JobList),
        other => Err(ForgeError::DslParse(format!(
            "job: unexpected rule {other:?}"
        ))),
    }
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

        Rule::vacuum_stmt => {
            let mut source = None;
            let mut keep = 0usize;
            let mut all = false;
            let mut apply = false;
            for part in pair.into_inner() {
                match part.as_rule() {
                    Rule::vacuum_source => {
                        let mut inner = part.into_inner();
                        source = Some(next_str(&mut inner, "vacuum: expected source name")?);
                    }
                    Rule::vacuum_keep => {
                        let n = part.into_inner().next().ok_or_else(|| {
                            ForgeError::DslParse("vacuum: expected KEEP number".into())
                        })?;
                        keep = n.as_str().parse().map_err(|_| {
                            ForgeError::DslParse("vacuum: invalid KEEP number".into())
                        })?;
                    }
                    Rule::vacuum_all => all = true,
                    Rule::vacuum_apply => apply = true,
                    _ => {}
                }
            }
            Ok(ForgeQLIR::Vacuum {
                source,
                keep,
                all,
                apply,
            })
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

        Rule::show_sources_stmt
        | Rule::show_branches_stmt
        | Rule::show_stats_stmt
        | Rule::show_context_stmt
        | Rule::show_signature_stmt
        | Rule::show_outline_stmt
        | Rule::show_members_stmt
        | Rule::show_body_stmt
        | Rule::show_callees_stmt
        | Rule::show_lines_stmt => parse_show_statement(pair),

        Rule::change_stmt => parse_change(pair),

        Rule::copy_stmt => parse_copy_or_move(pair, false),
        Rule::move_stmt => parse_copy_or_move(pair, true),

        Rule::find_stmt => parse_find(pair),

        Rule::find_node_stmt => {
            let node_id = next_str(&mut pair.into_inner(), "find_node: expected node_id")?;
            Ok(ForgeQLIR::FindNode { node_id })
        }

        Rule::change_node_stmt
        | Rule::change_nodes_last_stmt
        | Rule::insert_node_stmt
        | Rule::delete_node_stmt
        | Rule::show_node_stmt => parse_node_stmt(pair),
        Rule::show_more_stmt => parse_show_more_stmt(pair),
        Rule::undo_stmt => parse_undo_stmt(pair),
        Rule::job_stmt => parse_job_stmt(pair),
        Rule::export_patch_stmt => parse_export_patch_stmt(pair),

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
            let mut inner = pair.into_inner();
            let step = inner
                .next()
                .map(|l| unquote(l.as_str()))
                .ok_or_else(|| ForgeError::DslParse("verify: expected step name".into()))?;
            let args = inner.map(|l| unquote(l.as_str())).collect();
            Ok(ForgeQLIR::VerifyBuild { step, args })
        }

        Rule::run_stmt => {
            let mut inner = pair.into_inner();
            let step = inner
                .next()
                .map(|l| unquote(l.as_str()))
                .ok_or_else(|| ForgeError::DslParse("run: expected step name".into()))?;
            let args = inner.map(|l| unquote(l.as_str())).collect();
            Ok(ForgeQLIR::Run { step, args })
        }

        Rule::commit_stmt => {
            let message = unwrap_content(
                pair.into_inner()
                    .next()
                    .ok_or_else(|| ForgeError::DslParse("commit: expected message".into()))?,
            )?;
            Ok(ForgeQLIR::Commit { message })
        }

        r => Err(ForgeError::DslParse(format!("unhandled rule: {r:?}"))),
    }
}

/// Parse the read-only `SHOW ...` query family: sources, branches, stats,
/// context, signature, outline, members, body, callees, and lines. Dispatched
/// from `parse_statement`; `pair`'s rule is one of the `show_*_stmt` variants
/// listed there. (`SHOW NODE` and `SHOW MORE` are handled by `parse_node_stmt`
/// / `parse_show_more_stmt`, not here.)
fn parse_show_statement(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    match pair.as_rule() {
        Rule::show_sources_stmt => Ok(ForgeQLIR::ShowSources),

        Rule::show_branches_stmt => Ok(ForgeQLIR::ShowBranches),

        Rule::show_stats_stmt => {
            let session_id = pair.into_inner().next().map(|p| unquote(p.as_str()));
            Ok(ForgeQLIR::ShowStats { session_id })
        }

        Rule::show_context_stmt => {
            let (symbol, backend, clauses) =
                parse_symbol_backend_clauses(pair, "show_context: expected symbol name")?;
            Ok(ForgeQLIR::ShowContext {
                symbol,
                backend,
                clauses,
            })
        }

        Rule::show_signature_stmt => {
            let (symbol, backend, clauses) =
                parse_symbol_backend_clauses(pair, "show_signature: expected symbol name")?;
            Ok(ForgeQLIR::ShowSignature {
                symbol,
                backend,
                clauses,
            })
        }

        Rule::show_outline_stmt => {
            let mut inner = pair.into_inner();
            let file = next_str(&mut inner, "show_outline: expected file path")?;
            // Optional `ALL` keyword sits between the file and any USING / clauses.
            let all = inner
                .peek()
                .is_some_and(|p| p.as_rule() == Rule::outline_all);
            if all {
                let _ = inner.next();
            }
            let backend = parse_using_clause(&mut inner)?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowOutline {
                file,
                all,
                backend,
                clauses,
            })
        }

        Rule::show_members_stmt => {
            let (symbol, backend, clauses) =
                parse_symbol_backend_clauses(pair, "show_members: expected symbol name")?;
            Ok(ForgeQLIR::ShowMembers {
                symbol,
                backend,
                clauses,
            })
        }

        Rule::show_body_stmt => {
            let (symbol, backend, clauses) =
                parse_symbol_backend_clauses(pair, "show_body: expected symbol name")?;
            Ok(ForgeQLIR::ShowBody {
                symbol,
                backend,
                clauses,
            })
        }

        Rule::show_callees_stmt => {
            let (symbol, backend, clauses) =
                parse_symbol_backend_clauses(pair, "show_callees: expected symbol name")?;
            Ok(ForgeQLIR::ShowCallees {
                symbol,
                backend,
                clauses,
            })
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
            let backend = parse_using_clause(&mut inner)?;
            let clauses = parse_clauses(inner);
            Ok(ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                backend,
                clauses,
            })
        }

        r => Err(ForgeError::DslParse(format!(
            "parse_show_statement: unhandled rule: {r:?}"
        ))),
    }
}

/// Parse the `<symbol> [USING backend] [clauses]` tail shared by the SHOW
/// context / signature / members / body / callees statements.
fn parse_symbol_backend_clauses(
    pair: pest::iterators::Pair<'_, Rule>,
    ctx: &'static str,
) -> Result<(String, crate::ir::Backend, crate::ir::Clauses), ForgeError> {
    let mut inner = pair.into_inner();
    let symbol = next_str(&mut inner, ctx)?;
    let backend = parse_using_clause(&mut inner)?;
    let clauses = parse_clauses(inner);
    Ok((symbol, backend, clauses))
}

/// Extract `(pattern, replacement, word_boundary)` from a `change_matching`
/// pair: `MATCHING [WORD] 'pattern' WITH 'replacement'`.
fn parse_matching_parts(
    matching: pest::iterators::Pair<'_, Rule>,
) -> Result<(String, String, bool), ForgeError> {
    let mut m = matching.into_inner();
    let word_boundary = m.peek().is_some_and(|p| p.as_rule() == Rule::word_modifier);
    if word_boundary {
        let _ = m.next(); // consume the WORD token
    }
    let pattern = next_str(&mut m, "change_matching: expected pattern")?;
    let replacement = m
        .next()
        .ok_or_else(|| ForgeError::DslParse("change_matching: expected replacement".into()))
        .and_then(unwrap_content)?;
    Ok((pattern, replacement, word_boundary))
}
/// Parse `CHANGE NODE 'id' [IF REV] (WITH content | MATCHING [WORD] 'a' WITH 'b')`.
fn parse_change_node_stmt(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    let mut inner = pair.into_inner();
    let node_id = next_str(&mut inner, "change_node: expected node_id")?;
    let mut if_rev = None;
    let mut content = None;
    let mut matching = None;
    for child in inner {
        match child.as_rule() {
            Rule::if_rev_clause => {
                if_rev = child
                    .into_inner()
                    .next()
                    .map(unwrap_any_value)
                    .transpose()?;
            }
            Rule::change_matching => {
                matching = Some(parse_matching_parts(child)?);
            }
            Rule::content_value => {
                content = Some(unwrap_content(child)?);
            }
            _ => {}
        }
    }
    if let Some((pattern, replacement, word_boundary)) = matching {
        return Ok(ForgeQLIR::ChangeNodeMatching {
            node_id,
            if_rev,
            pattern,
            replacement,
            word_boundary,
        });
    }
    Ok(ForgeQLIR::ChangeNode {
        node_id,
        if_rev,
        content: content
            .ok_or_else(|| ForgeError::DslParse("change_node: missing content".into()))?,
    })
}
/// Parse the node-handle statements (CHANGE / INSERT / DELETE / SHOW NODE),
/// each addressing an indexed node by its stable id.
fn parse_node_stmt(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
    match pair.as_rule() {
        Rule::change_node_stmt => parse_change_node_stmt(pair),

        Rule::change_nodes_last_stmt => {
            let matching = pair.into_inner().next().ok_or_else(|| {
                ForgeError::DslParse("change_nodes_last: expected MATCHING clause".into())
            })?;
            let (pattern, replacement, word_boundary) = parse_matching_parts(matching)?;
            Ok(ForgeQLIR::ChangeNodesLast {
                pattern,
                replacement,
                word_boundary,
            })
        }
        Rule::insert_node_stmt => {
            let mut inner = pair.into_inner();
            let pos_pair = inner
                .next()
                .ok_or_else(|| ForgeError::DslParse("insert_node: expected position".into()))?;
            let before = pos_pair.as_str() == "BEFORE";
            let node_id = next_str(&mut inner, "insert_node: expected node_id")?;
            let content = inner
                .next()
                .ok_or_else(|| ForgeError::DslParse("insert_node: expected content".into()))
                .and_then(unwrap_content)?;
            Ok(ForgeQLIR::InsertNode {
                node_id,
                before,
                content,
            })
        }

        Rule::delete_node_stmt => {
            let mut inner = pair.into_inner();
            let node_id = next_str(&mut inner, "delete_node: expected node_id")?;
            let if_rev = inner
                .find(|p| p.as_rule() == Rule::if_rev_clause)
                .and_then(|p| p.into_inner().next())
                .map(unwrap_any_value)
                .transpose()?;
            Ok(ForgeQLIR::DeleteNode { node_id, if_rev })
        }

        Rule::show_node_stmt => {
            let mut inner = pair.into_inner();
            let node_id = next_str(&mut inner, "show_node: expected node_id")?;
            let mut metadata = false;
            let mut clauses = crate::ir::Clauses::default();
            for child in inner {
                match child.as_rule() {
                    Rule::show_node_mode => {
                        metadata = child.as_str() == "METADATA";
                    }
                    Rule::clauses => {
                        clauses = parse_clauses(child.into_inner());
                    }
                    _ => {}
                }
            }
            Ok(ForgeQLIR::ShowNode {
                node_id,
                metadata,
                clauses,
            })
        }

        r => Err(ForgeError::DslParse(format!(
            "parse_node_stmt: unhandled rule: {r:?}"
        ))),
    }
}

#[cfg(test)]
mod tests;
