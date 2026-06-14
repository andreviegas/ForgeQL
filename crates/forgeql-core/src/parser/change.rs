//! Parse `CHANGE CONTENT`, `COPY LINES`, `MOVE LINES`.
use super::Rule;
use super::clauses::parse_clauses;
use super::helpers::{next_str, unquote, unwrap_content};
use crate::error::ForgeError;
use crate::ir::{ChangeTarget, ForgeQLIR};
#[allow(clippy::too_many_lines)]
pub(super) fn parse_change(pair: pest::iterators::Pair<'_, Rule>) -> Result<ForgeQLIR, ForgeError> {
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
    let target = parse_change_target(target_inner)?;

    // Remaining pairs: the trailing `clauses` block.
    let clauses = parse_clauses(inner);

    Ok(ForgeQLIR::ChangeContent {
        files,
        target,
        clauses,
    })
}

/// Parse the next token as a 1-based line number. `missing` is the error when no
/// token is present; `label` prefixes a parse-failure error.
fn next_usize(
    m: &mut pest::iterators::Pairs<'_, Rule>,
    missing: &'static str,
    label: &'static str,
) -> Result<usize, ForgeError> {
    m.next()
        .ok_or_else(|| ForgeError::DslParse(missing.into()))?
        .as_str()
        .parse()
        .map_err(|e| ForgeError::DslParse(format!("{label}: {e}")))
}

/// Parse the inner `change_target` rule into a [`ChangeTarget`]. Dispatched from
/// `parse_change`; `target_inner` is the single sub-rule of the change target.
fn parse_change_target(
    target_inner: pest::iterators::Pair<'_, Rule>,
) -> Result<ChangeTarget, ForgeError> {
    let target = match target_inner.as_rule() {
        Rule::change_matching => {
            let mut m = target_inner.into_inner();
            // Check for optional WORD modifier.
            let word_boundary = m.peek().is_some_and(|p| p.as_rule() == Rule::word_modifier);
            if word_boundary {
                let _ = m.next(); // consume the WORD token
            }
            let pattern = next_str(&mut m, "change_matching: expected pattern")?;
            let replacement = m
                .next()
                .ok_or_else(|| ForgeError::DslParse("change_matching: expected replacement".into()))
                .and_then(unwrap_content)?;
            ChangeTarget::Matching {
                pattern,
                replacement,
                word_boundary,
            }
        }
        Rule::change_lines_delete => {
            let mut m = target_inner.into_inner();
            let start = next_usize(
                &mut m,
                "change_lines_delete: expected start",
                "change_lines_delete start",
            )?;
            let end = next_usize(
                &mut m,
                "change_lines_delete: expected end",
                "change_lines_delete end",
            )?;
            // Empty content replaces the line range with nothing (deletion).
            ChangeTarget::Lines {
                start,
                end,
                content: String::new(),
            }
        }
        Rule::change_lines_range => {
            let mut m = target_inner.into_inner();
            let start = next_usize(&mut m, "change_lines: expected start", "change_lines start")?;
            let end = next_usize(&mut m, "change_lines: expected end", "change_lines end")?;
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
    Ok(target)
}

/// Parse `COPY LINES n-m OF 'src' TO 'dst' [AT LINE k]` and
/// `MOVE LINES n-m OF 'src' TO 'dst' [AT LINE k]`.
///
/// `is_move` distinguishes the two variants.
pub(super) fn parse_copy_or_move(
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
