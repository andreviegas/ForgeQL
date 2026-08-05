//! Unit tests for the MCP server handler: the `get_info` capability surface,
//! `run_fql` dispatch and its error shapes, and the response footer and
//! `SHOW MORE` buffering that sit between the engine and the client.

use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::showmore::buffering_params;
use forgeql_lang_c::CLanguage;
use forgeql_lang_cpp::CppLanguage;
use tempfile::tempdir;

fn make_registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![
        Arc::new(CLanguage),
        Arc::new(CppLanguage),
    ]))
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

fn mcp_with_session() -> (ForgeQlMcp, String, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();
    let _ = fs::copy(
        src.join("motor_control.h"),
        dir.path().join("motor_control.h"),
    )
    .expect("copy .h");
    let _ = fs::copy(
        src.join("motor_control.cpp"),
        dir.path().join("motor_control.cpp"),
    )
    .expect("copy .cpp");

    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    // Register the session under the MCP auth user so run_fql can find it.
    let session_id = engine
        .register_local_session_for(auth(AuthContext::Mcp), dir.path())
        .expect("register session");
    let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);
    (mcp, session_id, dir)
}

fn first_text(result: &CallToolResult) -> &str {
    result.content[0]
        .as_text()
        .expect("expected text content")
        .text
        .as_str()
}

#[tokio::test]
async fn get_info_returns_tools_capability() {
    let tmp = tempdir().unwrap();
    let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);
    let info = mcp.get_info();
    assert!(info.capabilities.tools.is_some());
}

#[tokio::test]
async fn get_info_has_instructions() {
    let tmp = tempdir().unwrap();
    let engine = ForgeQLEngine::new(tmp.path().to_path_buf(), make_registry()).unwrap();
    let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);
    let info = mcp.get_info();
    let instructions = info.instructions.expect("should have instructions");
    assert!(instructions.contains("ForgeQL"));
}

#[tokio::test]
async fn run_fql_find_symbols() {
    let (mcp, session_id, _dir) = mcp_with_session();
    let result = mcp
        .run_fql(Parameters(RunFqlParams {
            fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
            session_id: Some(session_id),
            format: None,
        }))
        .await;
    let call_result = result.expect("should succeed");
    let text = first_text(&call_result);
    assert!(
        text.contains("encenderMotor"),
        "JSON should contain symbol: {text}"
    );
}

#[tokio::test]
async fn run_fql_invalid_syntax_returns_error() {
    let (mcp, session_id, _dir) = mcp_with_session();
    let result = mcp
        .run_fql(Parameters(RunFqlParams {
            fql: "NOT VALID FQL".to_string(),
            session_id: Some(session_id),
            format: None,
        }))
        .await;
    assert!(result.is_err(), "invalid FQL should return ErrorData");
}

/// A rejected `IF REV` guard is a structured self-healing payload the
/// agent parses — it must arrive as an error-flagged tool result, not as
/// a protocol error with the JSON buried in the message string.
#[tokio::test]
async fn structured_rejection_returns_error_flagged_tool_result() {
    let (mcp, session_id, _dir) = mcp_with_session();
    let result = mcp
        .run_fql(Parameters(RunFqlParams {
            fql: "DELETE NODE 'nffffffffffff.0001' IF REV 'h0000000000000000'".to_string(),
            session_id: Some(session_id),
            format: None,
        }))
        .await
        .expect("a structured rejection must be a tool result, not a protocol error");
    assert_eq!(result.is_error, Some(true), "result must be error-flagged");
    let text = first_text(&result);
    let payload: serde_json::Value =
        serde_json::from_str(text).expect("payload must be parseable JSON");
    assert_eq!(
        payload["error"], "node_not_found",
        "payload should be the structured rejection: {text}"
    );
}

#[tokio::test]
async fn run_fql_create_source_is_blocked() {
    let (mcp, _session_id, _dir) = mcp_with_session();
    let result = mcp
        .run_fql(Parameters(RunFqlParams {
            fql: "CREATE SOURCE 'evil' FROM 'https://example.com/repo.git'".to_string(),
            session_id: None,
            format: None,
        }))
        .await;
    assert!(result.is_err(), "CREATE SOURCE via MCP must be rejected");
    let err = result.unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("not permitted") || msg.contains("administrator"),
        "error should mention admin restriction: {msg}"
    );
}

#[tokio::test]
async fn run_fql_csv_format_returns_compact_output() {
    let (mcp, session_id, _dir) = mcp_with_session();
    let result = mcp
        .run_fql(Parameters(RunFqlParams {
            fql: "FIND symbols WHERE name LIKE 'encender%'".to_string(),
            session_id: Some(session_id),
            format: Some(OutputFormat::Csv),
        }))
        .await;
    let call_result = result.expect("should succeed");
    let text = first_text(&call_result);
    // Compact CSV: header row with op and total, schema hint, grouped data.
    assert!(
        text.contains("\"find_symbols\""),
        "compact output should have op in header: {text}"
    );
    assert!(
        text.contains("\"fql_kind\""),
        "compact output should have schema row: {text}"
    );
    assert!(
        text.contains("encenderMotor"),
        "compact output should contain symbol name: {text}"
    );
    // tokens_approx is size-gated (Phase 0 noise reduction): a small result
    // omits the footer. This query returns a handful of rows, well under the
    // threshold, so the footer must be absent.
    assert!(
        !text.contains("tokens_approx"),
        "small compact output should omit the tokens_approx footer: {text}"
    );
}

#[test]
fn append_meta_gates_footers_by_size() {
    // Small CSV output, no budget, below the token threshold → no footers.
    let small = super::append_meta("\"find_symbols\",0\n\"x\",1", None);
    assert!(
        !small.contains("tokens_approx"),
        "small output must omit tokens_approx: {small}"
    );
    assert!(
        !small.contains("line_budget"),
        "no budget configured must omit line_budget: {small}"
    );

    // Large CSV output (> threshold): tokens_approx is restored as a hint,
    // but the narrowing hint only fires past the big-response threshold.
    let big = super::append_meta(&format!("\"find_symbols\",0\n{}", "x".repeat(4096)), None);
    assert!(
        big.contains("tokens_approx"),
        "large output must keep tokens_approx as a narrowing hint"
    );
    assert!(
        !big.contains("\"hint\""),
        "mid-size output must not carry the narrowing hint: {}",
        &big[big.len().saturating_sub(200)..]
    );

    // Very large output (> big-response threshold): the narrowing hint
    // row is appended.
    let huge = super::append_meta(&format!("\"find_files\",0\n{}", "x".repeat(9000)), None);
    assert!(
        huge.contains("\"hint\",\"large response"),
        "huge output must carry the narrowing hint: {}",
        &huge[huge.len().saturating_sub(300)..]
    );

    // A budget line (the caller only passes Some when warning/critical) is
    // shown regardless of size.
    let budgeted = super::append_meta("\"x\",1", Some("12 (-5)"));
    assert!(
        budgeted.contains("line_budget"),
        "a low-budget line must be shown regardless of size: {budgeted}"
    );
}

#[test]
fn show_and_query_route_through_show_more_buffer() {
    use forgeql_core::result::{QueryResult, ShowContent, ShowResult, SourceLine};

    // FIND (Query) opts into buffering so large result sets page via SHOW MORE.
    let query = ForgeQLResult::Query(QueryResult {
        op: "find_symbols".to_string(),
        results: vec![],
        total: 0,
        metric_hint: None,
        group_by_field: None,
        hint: None,
        found_rev: None,
    });
    assert!(
        buffering_params(&query, 40).is_some(),
        "FIND output must route through the SHOW MORE buffer"
    );

    // A SHOW result with more lines than the cap is windowed inline while the
    // full rendered output is written to the session buffer for SHOW MORE,
    // replacing the old hard block that returned zero lines.
    let lines: Vec<SourceLine> = (1..=100)
        .map(|i| SourceLine {
            rev: None,
            line: i,
            text: format!("source line {i}"),
            marker: None,
            node_id: None,
            node_offset: None,
        })
        .collect();
    let show = ForgeQLResult::Show(ShowResult {
        op: "show_lines".to_string(),
        symbol: None,
        file: Some(PathBuf::from("big.rs")),
        start_line: Some(1),
        end_line: Some(100),
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Lines {
            lines,
            byte_start: None,
            depth: None,
        },
    });
    assert!(
        buffering_params(&show, 40).is_some(),
        "SHOW output must route through the SHOW MORE buffer"
    );

    let tmp = tempdir().expect("tempdir");
    let rendered = compact::to_compact(&show);
    let full_lines = rendered.lines().count();
    assert!(full_lines > 40, "fixture must exceed the cap: {full_lines}");

    let windowed = finalize_csv(rendered, &show, Some(tmp.path()), 40);
    let shown = windowed.lines().count();
    assert!(
        shown <= 42,
        "over-cap SHOW must be windowed near the cap, got {shown} lines"
    );
    assert!(
        windowed.contains("show_more"),
        "windowed output must carry the SHOW MORE hint: {windowed}"
    );

    // The full output is recoverable from the session buffer.
    let buffer = forgeql_core::showmore::read_buffer(tmp.path())
        .expect("read buffer")
        .expect("buffer must exist after an over-cap SHOW");
    assert!(
        buffer.total() >= full_lines,
        "buffer must hold the full rendered output"
    );
}

#[test]
fn show_more_output_is_never_rebuffered() {
    use forgeql_core::result::{ShowContent, ShowResult};

    // Paging an existing buffer must not re-buffer its own rendering:
    // doing so rotates the ring away from the real buffer (so the next
    // SHOW MORE stops advancing) and re-escapes the already-rendered CSV
    // lines, which compounds on every call. A `show_more` result opts OUT.
    let paged = ForgeQLResult::Show(ShowResult {
        op: "show_more".to_string(),
        symbol: Some("show".to_string()),
        file: None,
        start_line: None,
        end_line: None,
        total_lines: None,
        hint: None,
        metadata: None,
        content: ShowContent::Lines {
            lines: vec![],
            byte_start: None,
            depth: None,
        },
    });
    assert!(
        buffering_params(&paged, 40).is_none(),
        "SHOW MORE output must not re-buffer (would compound escaping and stall paging)"
    );
}

/// Automotive structured-XML end-to-end through the MCP tool: AUTOSAR
/// ECUC parameter values and tresos datamodel entries are findable by
/// their real names — the discovery half of the workflow that replaces
/// GUI round-trips through vendor configuration tools.
#[tokio::test]
async fn run_fql_automotive_xml_find_by_real_names() {
    use forgeql_lang_text::xml::XmlLanguage;

    let dir = tempdir().expect("tempdir");
    let src = fixtures_dir();
    for fixture in ["EcucCanIf.arxml", "TresosAdc.xdm"] {
        let _ = fs::copy(src.join(fixture), dir.path().join(fixture)).expect("copy fixture");
    }
    let registry = Arc::new(LanguageRegistry::new(vec![
        Arc::new(CLanguage),
        Arc::new(CppLanguage),
        Arc::new(XmlLanguage),
    ]));
    let mut engine = ForgeQLEngine::new(dir.path().join("data"), registry).expect("engine");
    let session_id = engine
        .register_local_session_for(auth(AuthContext::Mcp), dir.path())
        .expect("register session");
    let mcp = ForgeQlMcp::new(Arc::new(TokioMutex::new(engine)), None);

    let run = |fql: &str, sid: String| {
        mcp.run_fql(Parameters(RunFqlParams {
            fql: fql.to_string(),
            session_id: Some(sid),
            format: None,
        }))
    };

    // An ECUC parameter value carries no SHORT-NAME and no identifying
    // attribute; it must be findable by its DEFINITION-REF's last path
    // segment. (Node-handle mutation on XML elements is covered by the
    // golden node-mutation suites against the git-backed corpora.)
    let result = run(
        "FIND symbols WHERE name = 'CanIfPublicTxBuffering'",
        session_id.clone(),
    )
    .await
    .expect("find param should succeed");
    let text = first_text(&result);
    assert!(
        text.contains("CanIfPublicTxBuffering") && text.contains("EcucCanIf.arxml"),
        "ECUC param must be findable by name: {text}"
    );

    // A deeply nested sub-container keeps its SHORT-NAME identity.
    let result = run(
        "FIND symbols WHERE name = 'CanIfBufferCfg_0'",
        session_id.clone(),
    )
    .await
    .expect("find container should succeed");
    let text = first_text(&result);
    assert!(
        text.contains("CanIfBufferCfg_0"),
        "nested container findable by SHORT-NAME: {text}"
    );

    // A tresos datamodel variable is named by its `name` attribute, deep
    // inside namespaced (`d:ctr`/`d:var`) nesting.
    let result = run("FIND symbols WHERE name = 'AdcPrescale'", session_id)
        .await
        .expect("find tresos var should succeed");
    let text = first_text(&result);
    assert!(text.contains("AdcPrescale"), "tresos var findable: {text}");
    assert!(text.contains("TresosAdc.xdm"), "path present: {text}");
}
