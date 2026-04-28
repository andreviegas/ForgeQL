use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::{
    ast::{query, show},
    ir::{Clauses, ForgeQLIR},
    result::{FileEntry, ForgeQLResult, ShowContent},
    session::Session,
};

use super::ForgeQLEngine;
use super::{
    DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, convert_show_json, reject_text_filter,
    resolve_body_symbol, resolve_symbol, resolve_type_symbol,
};

impl ForgeQLEngine {
    #[allow(clippy::too_many_lines)]
    pub(super) fn exec_show(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (workspace, index) = self.require_workspace_and_index(session_id)?;
        let root = workspace.root();

        let json = match op {
            ForgeQLIR::ShowContext { symbol, clauses } => {
                let context_lines = clauses.depth.unwrap_or(DEFAULT_CONTEXT_LINES);
                resolve_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_context(def, index, &workspace, symbol, context_lines)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowSignature { symbol, clauses } => {
                resolve_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_signature(def, index, &workspace, symbol, &self.lang_registry)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowOutline { file, .. } => show::show_outline(index, &workspace, file)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowMembers { symbol, clauses } => {
                resolve_type_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_members(def, index, &workspace, symbol, &self.lang_registry)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowBody { symbol, clauses } => {
                resolve_body_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_body(
                            def,
                            index,
                            &workspace,
                            symbol,
                            Some(clauses.depth.unwrap_or(DEFAULT_BODY_DEPTH)),
                            &self.lang_registry,
                        )
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowCallees { symbol, clauses } => {
                resolve_body_symbol(index, symbol, clauses, root)
                    .and_then(|def| {
                        show::show_callees(def, index, &workspace, symbol, &self.lang_registry)
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => show::show_lines(&workspace, file, *start_line, *end_line)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::FindFiles { clauses } => {
                reject_text_filter(clauses)?;
                let glob = clauses.in_glob.as_deref().unwrap_or("**");
                // IN / EXCLUDE are applied by find_files(); build typed entries
                // so the full clause pipeline (WHERE, GROUP BY, HAVING, ORDER BY,
                // LIMIT, OFFSET) can run against individual file rows.
                let raw = query::find_files(&workspace, glob, clauses.exclude_glob.as_deref());
                let mut entries: Vec<FileEntry> = raw
                    .iter()
                    .filter_map(|v| {
                        let path = v.get("path").and_then(|p| p.as_str()).map(PathBuf::from)?;
                        let extension = v
                            .get("extension")
                            .and_then(|e| e.as_str())
                            .unwrap_or("")
                            .to_string();
                        let size = v
                            .get("size")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0);
                        Some(FileEntry {
                            path,
                            extension,
                            size,
                            depth: None,
                            count: None,
                        })
                    })
                    .collect();
                // Apply the full clause pipeline.  IN / EXCLUDE are already
                // handled above so they become no-ops here; GROUP BY, HAVING,
                // WHERE, ORDER BY, LIMIT, OFFSET all run normally.
                crate::filter::apply_clauses(&mut entries, clauses);
                // Always show individual files — the LIMIT clause (default 20)
                // already caps how many entries are returned.  Previously, when
                // no IN was specified the depth was set to DEFAULT_BODY_DEPTH (0)
                // which collapsed everything into a single '/' directory entry.
                let max_depth = clauses.depth.unwrap_or(usize::MAX);
                // When GROUP BY was requested the pipeline has already
                // aggregated entries and stored per-group counts; skip the
                // depth-grouping step so those results are not disturbed.
                let results: Vec<serde_json::Value> = if clauses.group_by.is_some() {
                    entries
                        .iter()
                        .map(|fe| {
                            let mut obj = serde_json::json!({
                                "path":      fe.path.display().to_string(),
                                "extension": fe.extension,
                                "size":      fe.size,
                            });
                            if let Some(n) = fe.count {
                                obj["count"] = serde_json::Value::from(n);
                            }
                            obj
                        })
                        .collect()
                } else {
                    let file_json: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|fe| {
                            serde_json::json!({
                                "path":      fe.path.display().to_string(),
                                "extension": fe.extension,
                                "size":      fe.size,
                            })
                        })
                        .collect();
                    query::group_files_by_depth(&file_json, max_depth)
                };
                let count = results.len();
                serde_json::json!({
                    "op":      "find_files",
                    "glob":    glob,
                    "depth":   max_depth,
                    "results": results,
                    "count":   count,
                })
            }
            other => serde_json::json!({ "error": format!("not a show op: {other:?}") }),
        };

        // Check for error responses.
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            bail!("{err}");
        }

        // Convert the JSON value to a typed ShowResult.
        let mut show_result = convert_show_json(op, &json)?;

        // Apply the full clause pipeline (WHERE, ORDER BY, LIMIT, OFFSET, …)
        // to structured list results: outline, members, and call graph entries.
        match (&mut show_result.content, op) {
            (ShowContent::Outline { entries }, ForgeQLIR::ShowOutline { clauses, .. }) => {
                crate::filter::apply_clauses(entries, clauses);
            }
            (ShowContent::Members { members, .. }, ForgeQLIR::ShowMembers { clauses, .. }) => {
                crate::filter::apply_clauses(members, clauses);
            }
            (ShowContent::CallGraph { entries, .. }, ForgeQLIR::ShowCallees { clauses, .. }) => {
                crate::filter::apply_clauses(entries, clauses);
            }
            _ => {}
        }

        // Extract clauses for ShowContent::Lines variants.
        let show_clauses: Option<&Clauses> = match op {
            ForgeQLIR::ShowBody { clauses, .. }
            | ForgeQLIR::ShowLines { clauses, .. }
            | ForgeQLIR::ShowContext { clauses, .. } => Some(clauses),
            _ => None,
        };

        // SHOW LINES n-m has a user-specified range — the implicit 40-line cap
        // should NOT block it.  Only SHOW body / SHOW context are subject to
        // the implicit cap (they can produce unbounded output).
        let is_explicit_range = matches!(op, ForgeQLIR::ShowLines { .. });

        // Apply WHERE predicates BEFORE the line caps.
        // This lets queries like `SHOW body OF 'fn' WHERE text MATCHES 'TODO'`
        // filter over the full function body, not just the first N lines.
        if let (ShowContent::Lines { lines, .. }, Some(clauses)) =
            (&mut show_result.content, show_clauses)
        {
            for predicate in &clauses.where_predicates {
                let pred = predicate.clone();
                lines.retain(|line| crate::filter::eval_predicate(line, &pred));
            }
        }

        // Apply all caps: explicit LIMIT/OFFSET, implicit DEFAULT_SHOW_LINE_LIMIT,
        // and budget critical cap.
        let budget_max = session_id
            .and_then(|sid| self.sessions.get(sid))
            .and_then(Session::budget_critical_max_lines);
        Self::apply_show_lines_cap(
            &mut show_result,
            show_clauses,
            budget_max,
            is_explicit_range,
        );

        Ok(ForgeQLResult::Show(show_result))
    }
}
