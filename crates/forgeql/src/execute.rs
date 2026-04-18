//! FQL execution helpers used by all run-modes.
//!
//! [`execute_and_print`] is the shared workhorse — it resolves input, parses,
//! runs each statement through the engine, formats output, and logs.
//! It is intentionally decomposed into three pure sub-functions so each
//! concern can be unit-tested in isolation.

use forgeql_core::compact;
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::parser;
use forgeql_core::query_logger::QueryLogger;
use forgeql_core::result::ForgeQLResult;
use tracing::info;

use crate::cli::CliFormat;
use crate::session::SessionFile;

// -----------------------------------------------------------------------
// Sub-functions (pure / easily testable)
// -----------------------------------------------------------------------

/// Resolve the FQL input text.
///
/// If `input` looks like a path to an `.fql` file (case-insensitive
/// extension check) **and** the file exists on disk, the file contents
/// are returned.  Otherwise `input` itself is returned as-is.
///
/// # Errors
/// Returns an error only when the path resolves to an `.fql` file but
/// the file cannot be read.
pub(crate) fn resolve_fql_input(input: &str) -> Result<String, String> {
    let path = std::path::Path::new(input);
    if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("fql"))
        && path.exists()
    {
        std::fs::read_to_string(path).map_err(|err| format!("error reading '{input}': {err}"))
    } else {
        Ok(input.to_string())
    }
}

/// Update `session` and `log_source` from a completed FQL result.
///
/// Called after every successful engine execution.  Handles two cases:
///
/// * `UseSource` IR — captures `session_id`, `source`, `branch`,
///   `as_branch` so the session can be resumed next invocation.
/// * `CreateSource` IR — updates `log_source` to the new source name.
pub(crate) fn update_session_from_result(
    session: &mut SessionFile,
    op: &ForgeQLIR,
    result: &ForgeQLResult,
    log_source: &mut String,
) {
    let ForgeQLResult::SourceOp(sop) = result else {
        return;
    };
    if let Some(ref sid) = sop.session_id {
        session.session_id = Some(sid.clone());
        info!(%sid, "session started");
    }
    if let ForgeQLIR::UseSource {
        source,
        branch,
        as_branch,
    } = op
    {
        session.source = Some(source.clone());
        session.branch = Some(branch.clone());
        session.as_branch = Some(as_branch.clone());
        log_source.clone_from(source);
    }
    if let ForgeQLIR::CreateSource { name, .. } = op {
        log_source.clone_from(name);
    }
}

/// Render a `ForgeQLResult` to a string using the requested format.
pub(crate) fn format_result(result: &ForgeQLResult, format: CliFormat) -> String {
    match format {
        CliFormat::Text => format!("{result}"),
        CliFormat::Compact => compact::to_compact(result),
        CliFormat::Json => result.to_json_pretty(),
    }
}

// -----------------------------------------------------------------------
// Main entry point used by all runners
// -----------------------------------------------------------------------

/// Parse `fql`, execute every statement through `engine`, and print
/// each result to stdout.  Parse/engine errors are printed to stderr.
///
/// * File-path inputs ending in `.fql` are read from disk first.
/// * Session state is updated in-place after each `USE`/`CREATE SOURCE`.
/// * When `logger` is `Some`, each statement is appended to the query log.
pub(crate) fn execute_and_print(
    engine: &mut ForgeQLEngine,
    fql: &str,
    session: &mut SessionFile,
    logger: Option<&mut QueryLogger>,
    format: CliFormat,
) {
    let fql_text = match resolve_fql_input(fql) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("{err}");
            return;
        }
    };

    let ops = match parser::parse_with_source(&fql_text) {
        Ok(ops) => ops,
        Err(err) => {
            eprintln!("parse error: {err}");
            return;
        }
    };

    // Consume the Option<&mut QueryLogger> so it can be used across the
    // loop without re-borrowing on each iteration.
    let mut log = logger;
    let mut log_source = session
        .source
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    for (source_text, op) in &ops {
        let t0 = std::time::Instant::now();
        match engine.execute(session.session_id.as_deref(), op) {
            Ok(result) => {
                let elapsed_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
                update_session_from_result(session, op, &result, &mut log_source);
                let output = format_result(&result, format);
                if let Some(ref mut l) = log {
                    l.log(source_text, &result, &output, elapsed_ms, &log_source, None);
                }
                println!("{output}");
            }
            Err(err) => {
                eprintln!("error: {err:#}");
            }
        }
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::unwrap_in_result,
    clippy::panic
)]
mod tests {
    use super::*;
    use forgeql_core::result::{QueryResult, SourceOpResult};
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ------------------------------------------------------------------
    // resolve_fql_input
    // ------------------------------------------------------------------

    #[test]
    fn resolve_returns_string_unchanged_when_not_fql_path() {
        let result = resolve_fql_input("FIND symbols WHERE name = 'main'");
        assert_eq!(result.unwrap(), "FIND symbols WHERE name = 'main'");
    }

    #[test]
    fn resolve_returns_string_unchanged_when_fql_extension_but_no_file() {
        // A path with .fql extension that does not exist is treated as raw FQL.
        let result = resolve_fql_input("/nonexistent/path/to/query.fql");
        assert_eq!(result.unwrap(), "/nonexistent/path/to/query.fql");
    }

    #[test]
    fn resolve_reads_fql_file_when_exists() {
        let mut tmp = NamedTempFile::with_suffix(".fql").unwrap();
        write!(tmp, "FIND symbols").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let result = resolve_fql_input(&path);
        assert_eq!(result.unwrap(), "FIND symbols");
    }

    #[test]
    fn resolve_fql_extension_is_case_insensitive() {
        let mut tmp = NamedTempFile::with_suffix(".FQL").unwrap();
        write!(tmp, "SHOW SOURCES").unwrap();
        let path = tmp.path().to_str().unwrap().to_string();

        let result = resolve_fql_input(&path);
        assert_eq!(result.unwrap(), "SHOW SOURCES");
    }

    #[test]
    fn resolve_returns_ok_for_txt_extension_path() {
        // .txt path should never be treated as a file read — returned as-is.
        let result = resolve_fql_input("some/query.txt");
        assert_eq!(result.unwrap(), "some/query.txt");
    }

    // ------------------------------------------------------------------
    // update_session_from_result
    // ------------------------------------------------------------------

    fn make_source_op_result(session_id: Option<&str>) -> ForgeQLResult {
        ForgeQLResult::SourceOp(SourceOpResult {
            op: "use_source".to_string(),
            source_name: None,
            session_id: session_id.map(str::to_string),
            branches: vec![],
            symbols_indexed: None,
            resumed: false,
            message: None,
        })
    }

    fn make_query_result() -> ForgeQLResult {
        ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results: vec![],
            total: 0,
            metric_hint: None,
            group_by_field: None,
        })
    }

    #[test]
    fn update_session_noop_for_non_source_op_result() {
        let mut session = SessionFile::default();
        let mut log_source = "prev".to_string();
        let op = ForgeQLIR::ShowSources;
        update_session_from_result(&mut session, &op, &make_query_result(), &mut log_source);
        // Nothing should change.
        assert!(session.session_id.is_none());
        assert_eq!(log_source, "prev");
    }

    #[test]
    fn update_session_sets_session_id_from_source_op() {
        let mut session = SessionFile::default();
        let mut log_source = "unknown".to_string();
        let op = ForgeQLIR::ShowSources;
        let result = make_source_op_result(Some("new-sid"));
        update_session_from_result(&mut session, &op, &result, &mut log_source);
        assert_eq!(session.session_id.as_deref(), Some("new-sid"));
    }

    #[test]
    fn update_session_does_not_set_session_id_when_none_in_op() {
        let mut session = SessionFile {
            session_id: Some("old".into()),
            ..Default::default()
        };
        let mut log_source = "unknown".to_string();
        let op = ForgeQLIR::ShowSources;
        let result = make_source_op_result(None); // session_id is None in result
        update_session_from_result(&mut session, &op, &result, &mut log_source);
        // session_id should remain unchanged (not cleared by None).
        assert_eq!(session.session_id.as_deref(), Some("old"));
    }

    #[test]
    fn update_session_captures_source_branch_as_branch_from_use_source() {
        let mut session = SessionFile::default();
        let mut log_source = "unknown".to_string();
        let op = ForgeQLIR::UseSource {
            source: "my-repo".into(),
            branch: "main".into(),
            as_branch: "agent-session".into(),
        };
        let result = make_source_op_result(Some("sid-1"));
        update_session_from_result(&mut session, &op, &result, &mut log_source);
        assert_eq!(session.source.as_deref(), Some("my-repo"));
        assert_eq!(session.branch.as_deref(), Some("main"));
        assert_eq!(session.as_branch.as_deref(), Some("agent-session"));
        assert_eq!(log_source, "my-repo");
    }

    #[test]
    fn update_session_updates_log_source_for_create_source() {
        let mut session = SessionFile::default();
        let mut log_source = "old-source".to_string();
        let op = ForgeQLIR::CreateSource {
            name: "new-repo".into(),
            url: "https://example.com/repo.git".into(),
        };
        let result = make_source_op_result(None);
        update_session_from_result(&mut session, &op, &result, &mut log_source);
        assert_eq!(log_source, "new-repo");
    }

    #[test]
    fn update_session_create_source_does_not_set_session_fields() {
        let mut session = SessionFile::default();
        let mut log_source = "old".to_string();
        let op = ForgeQLIR::CreateSource {
            name: "new-repo".into(),
            url: "https://example.com/repo.git".into(),
        };
        let result = make_source_op_result(None);
        update_session_from_result(&mut session, &op, &result, &mut log_source);
        // source/branch/as_branch must NOT be set by CreateSource.
        assert!(session.source.is_none());
        assert!(session.branch.is_none());
        assert!(session.as_branch.is_none());
    }

    // ------------------------------------------------------------------
    // format_result
    // ------------------------------------------------------------------

    fn make_empty_query_result() -> ForgeQLResult {
        make_query_result()
    }

    #[test]
    fn format_result_text_uses_display() {
        let result = make_empty_query_result();
        let text = format_result(&result, CliFormat::Text);
        let expected = format!("{result}");
        assert_eq!(text, expected);
    }

    #[test]
    fn format_result_compact_returns_csv_header() {
        let result = make_empty_query_result();
        let compact = format_result(&result, CliFormat::Compact);
        // Compact CSV always starts with the op name quoted.
        assert!(
            compact.contains("find_symbols"),
            "compact output: {compact}"
        );
    }

    #[test]
    fn format_result_json_returns_valid_json() {
        let result = make_empty_query_result();
        let json = format_result(&result, CliFormat::Json);
        // Must be parseable JSON.
        let parsed: serde_json::Value = serde_json::from_str(&json)
            .unwrap_or_else(|e| panic!("JSON parse error: {e}\noutput: {json}"));
        assert!(parsed.is_object());
    }

    #[test]
    fn format_result_json_differs_from_compact() {
        let result = make_empty_query_result();
        let compact = format_result(&result, CliFormat::Compact);
        let json = format_result(&result, CliFormat::Json);
        assert_ne!(compact, json);
    }
}
