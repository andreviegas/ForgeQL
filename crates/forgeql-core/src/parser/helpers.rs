//! Shared parser utilities: string extraction, unquoting, heredoc parsing, error enrichment.
use super::Rule;
use crate::error::ForgeError;
/// Advance `pairs` and return the next item as an unquoted `String`.
pub(super) fn next_str(
    pairs: &mut pest::iterators::Pairs<'_, Rule>,
    msg: &'static str,
) -> Result<String, ForgeError> {
    pairs
        .next()
        .map(|p| unquote(p.as_str()))
        .ok_or_else(|| ForgeError::DslParse(msg.into()))
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
