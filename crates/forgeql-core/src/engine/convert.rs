//! JSON → typed-result converters used by `exec_show` and `exec_change`.

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::ir::ForgeQLIR;
use crate::result::{
    CallDirection, CallGraphEntry, FileEntry, MemberEntry, OutlineEntry, ShowContent, ShowResult,
    SourceLine, SuggestionEntry,
};
use crate::transforms::TransformPlan;

// -----------------------------------------------------------------------
// Public converters
// -----------------------------------------------------------------------

/// Convert `TransformPlan` suggestions into typed `SuggestionEntry` values.
pub(crate) fn convert_suggestions(plan: &TransformPlan) -> Vec<SuggestionEntry> {
    plan.suggestions
        .iter()
        .map(|candidate| SuggestionEntry {
            path: candidate.path.clone(),
            byte_offset: candidate.byte_offset,
            snippet: candidate.snippet.clone(),
            reason: format!("{:?}", candidate.reason),
        })
        .collect()
}

/// Convert the JSON output from `execute_show` to a typed `ShowResult`.
///
/// This is a transitional bridge: the executor currently returns
/// `serde_json::Value`.  As we refactor `ast/show.rs` to return typed
/// results directly, this function will shrink and eventually disappear.
pub(crate) fn convert_show_json(op: &ForgeQLIR, json: &serde_json::Value) -> Result<ShowResult> {
    let op_name = json
        .get("op")
        .and_then(|v| v.as_str())
        .unwrap_or("show")
        .to_string();

    let symbol = json
        .get("symbol")
        .and_then(|v| v.as_str())
        .map(String::from);

    let file = json.get("file").and_then(|v| v.as_str()).map(PathBuf::from);

    let content = convert_show_content(op, json)?;

    let start_line = json
        .get("start_line")
        .and_then(serde_json::Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let end_line = json
        .get("end_line")
        .and_then(serde_json::Value::as_u64)
        .map(|n| usize::try_from(n).unwrap_or(usize::MAX));

    let metadata = json
        .get("metadata")
        .and_then(serde_json::Value::as_object)
        .cloned();

    Ok(ShowResult {
        op: op_name,
        symbol,
        file,
        content,
        start_line,
        end_line,
        total_lines: None,
        hint: None,
        metadata,
    })
}

// -----------------------------------------------------------------------
// Private helpers
// -----------------------------------------------------------------------

/// Convert the inner content of a SHOW JSON response to a typed `ShowContent`.
///
/// Each SHOW variant has a different JSON shape; this function pattern-matches
/// on the `ForgeQLIR` variant to determine how to interpret the JSON.
///
/// This is a transitional bridge — the executor currently returns
/// `serde_json::Value`.  The `u64 as usize` casts are safe because line
/// numbers and byte offsets never exceed `usize::MAX` on any supported target.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::redundant_closure_for_method_calls
)]
fn convert_show_content(op: &ForgeQLIR, json: &serde_json::Value) -> Result<ShowContent> {
    match op {
        // Line-oriented results: show_body, show_context, show_lines.
        ForgeQLIR::ShowBody { clauses, .. } => {
            let lines = extract_source_lines(json);
            Ok(ShowContent::Lines {
                lines,
                byte_start: json
                    .get("byte_start")
                    .and_then(|v| v.as_u64())
                    .map(|b| b as usize),
                depth: clauses.depth,
            })
        }

        ForgeQLIR::ShowContext { .. } | ForgeQLIR::ShowLines { .. } => {
            let lines = extract_source_lines(json);
            Ok(ShowContent::Lines {
                lines,
                byte_start: None,
                depth: None,
            })
        }

        ForgeQLIR::ShowSignature { .. } => {
            let signature = json
                .get("signature")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let line = json.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let byte_start = json.get("byte_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Ok(ShowContent::Signature {
                signature,
                line,
                byte_start,
            })
        }

        ForgeQLIR::ShowOutline { .. } => {
            let entries = json
                .get("results")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            Some(OutlineEntry {
                                name: entry.get("name")?.as_str()?.to_string(),
                                fql_kind: entry.get("fql_kind")?.as_str()?.to_string(),
                                path: PathBuf::from(entry.get("path")?.as_str()?),
                                line: entry.get("line")?.as_u64()? as usize,
                                node_id: entry
                                    .get("node_id")
                                    .and_then(|v| v.as_str())
                                    .map(String::from),
                                depth: entry
                                    .get("depth")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0) as usize,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(ShowContent::Outline { entries })
        }

        ForgeQLIR::ShowMembers { .. } => {
            let members = json
                .get("members")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|m| {
                            Some(MemberEntry {
                                fql_kind: m.get("fql_kind")?.as_str()?.to_string(),
                                text: m.get("text")?.as_str()?.to_string(),
                                line: m.get("line")?.as_u64()? as usize,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let byte_start = json.get("byte_start").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            Ok(ShowContent::Members {
                members,
                byte_start,
            })
        }

        ForgeQLIR::ShowCallees { .. } => {
            let direction = CallDirection::Callees;
            let entries = json
                .get("results")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            Some(CallGraphEntry {
                                name: entry.get("name")?.as_str()?.to_string(),
                                path: entry
                                    .get("path")
                                    .and_then(|v| v.as_str())
                                    .map(PathBuf::from),
                                line: entry
                                    .get("line")
                                    .and_then(|v| v.as_u64())
                                    .map(|l| l as usize),
                                byte_start: entry
                                    .get("byte_start")
                                    .and_then(|v| v.as_u64())
                                    .map(|b| b as usize),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(ShowContent::CallGraph { direction, entries })
        }

        ForgeQLIR::FindFiles { clauses, .. } => {
            let results = json
                .get("results")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|entry| {
                            // FIND files results can be strings or objects with "path".
                            let path_str = entry
                                .as_str()
                                .or_else(|| entry.get("path").and_then(|v| v.as_str()))?;
                            let extension = entry
                                .get("extension")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let size = entry.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                            let count = entry
                                .get("count")
                                .and_then(|v| v.as_u64())
                                .map(|n| n as usize);
                            Some(FileEntry {
                                path: PathBuf::from(path_str),
                                depth: clauses.depth,
                                extension,
                                size,
                                count,
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let total = json
                .get("count")
                .and_then(|v| v.as_u64())
                .unwrap_or(results.len() as u64) as usize;
            Ok(ShowContent::FileList {
                files: results,
                total,
            })
        }

        _ => bail!("unsupported SHOW variant: {op:?}"),
    }
}

/// Extract source lines from the JSON `"lines"` or `"results"` array.
#[allow(
    clippy::cast_possible_truncation,
    clippy::redundant_closure_for_method_calls
)]
fn extract_source_lines(json: &serde_json::Value) -> Vec<SourceLine> {
    // Different SHOW ops use different keys: "lines" or "results".
    let arr = json
        .get("lines")
        .or_else(|| json.get("results"))
        .and_then(|v| v.as_array());

    let Some(arr) = arr else {
        return Vec::new();
    };

    arr.iter()
        .filter_map(|item| {
            let line = item
                .get("line")
                .or_else(|| item.get("line_number"))
                .and_then(|v| v.as_u64())? as usize;
            let text = item
                .get("text")
                .or_else(|| item.get("content"))
                .and_then(|v| v.as_str())?
                .to_string();
            let marker = item
                .get("marker")
                .and_then(|v| v.as_str())
                .map(String::from);
            let node_id = item
                .get("node_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let node_offset = item
                .get("offset")
                .and_then(serde_json::Value::as_u64)
                .and_then(|n| usize::try_from(n).ok());
            Some(SourceLine {
                line,
                text,
                marker,
                node_id,
                node_offset,
            })
        })
        .collect()
}
