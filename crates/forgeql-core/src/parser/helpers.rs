//! Shared parser utilities: string extraction, unquoting, heredoc parsing, error enrichment.
use super::Rule;
use crate::error::ForgeError;
/// Extract a string from an `any_value` pair, handling heredoc, quoted, or bare tokens.
///
/// This is the single canonical extractor for any position that accepts `any_value`
/// in the grammar.  It covers all three variants:
/// - `heredoc_literal` — extracts the body between the open/close tags
/// - `string_literal`  — strips surrounding single or double quotes
/// - `bare_value`      — returns the token as-is
pub(super) fn unwrap_any_value(
    pair: pest::iterators::Pair<'_, Rule>,
) -> Result<String, ForgeError> {
    // any_value is a compound rule — drill to its single inner child.
    let inner = match pair.as_rule() {
        Rule::any_value => pair
            .into_inner()
            .next()
            .ok_or_else(|| ForgeError::DslParse("any_value: empty".into()))?,
        // Already the inner rule (bare_value, string_literal, heredoc_literal).
        _ => pair,
    };
    match inner.as_rule() {
        Rule::heredoc_literal => extract_heredoc(inner),
        Rule::string_literal => Ok(unquote(inner.as_str())),
        Rule::bare_value => Ok(inner.as_str().to_owned()),
        r => Err(ForgeError::DslParse(format!(
            "unwrap_any_value: unexpected {r:?}"
        ))),
    }
}

/// Advance `pairs` and return the next item as an unquoted `String`.
///
/// Handles heredoc, quoted, and bare tokens via [`unwrap_any_value`].
pub(super) fn next_str(
    pairs: &mut pest::iterators::Pairs<'_, Rule>,
    msg: &'static str,
) -> Result<String, ForgeError> {
    pairs
        .next()
        .ok_or_else(|| ForgeError::DslParse(msg.into()))
        .and_then(unwrap_any_value)
}
/// Strip the surrounding single-quotes from a `string_literal` token.
pub(super) fn unquote(s: &str) -> String {
    s.trim_matches(|c: char| c == '\'' || c == '"').to_string()
}

/// Extract the string content from a `content_value` pair, handling both
/// single-quoted string literals and heredoc blocks.
pub(super) fn unwrap_content(pair: pest::iterators::Pair<Rule>) -> Result<String, ForgeError> {
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
pub(super) fn extract_heredoc(pair: pest::iterators::Pair<Rule>) -> Result<String, ForgeError> {
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
// Backend (USING clause) extraction
// -----------------------------------------------------------------------

/// Peek at the next pair in `pairs`; if it is a `using_clause`, consume it
/// and return the parsed [`crate::ir::Backend`].
///
/// When the next pair is anything other than `using_clause` (or there is no
/// next pair), returns [`crate::ir::Backend::Default`] without consuming.
///
/// Callers must call this **after** extracting the primary target (symbol /
/// file / etc.) and **before** calling `parse_clauses`.
pub(super) fn parse_using_clause(
    pairs: &mut pest::iterators::Pairs<'_, Rule>,
) -> Result<crate::ir::Backend, crate::error::ForgeError> {
    if pairs
        .peek()
        .is_some_and(|p| p.as_rule() == Rule::using_clause)
    {
        // SAFETY: peek returned Some, so next() cannot return None.
        let clause_pair = pairs.next().unwrap_or_else(|| unreachable!());
        // using_clause contains a single string_literal child
        let name = clause_pair
            .into_inner()
            .next()
            .map(|p| unquote(p.as_str()))
            .unwrap_or_default();
        crate::ir::Backend::from_clause(&name)
    } else {
        Ok(crate::ir::Backend::Default)
    }
}
// Error enrichment
// -----------------------------------------------------------------------

/// Detect common SQL-isms that don't exist in `ForgeQL` and append a helpful
/// hint to the pest parse error.
pub(super) fn enrich_parse_error(input: &str, mut msg: String) -> String {
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
    if upper.starts_with("USE ") && !upper.contains(" AS ") {
        msg.push_str(
            "\n\nHint: USE requires an AS clause to name the session.\n\
             Example: USE source.branch AS 'my-session'",
        );
    }
    msg
}
