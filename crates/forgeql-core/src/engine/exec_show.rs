use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    ast::{parse_cache::CachedParse, query, show},
    ir::{Backend, Clauses, ForgeQLIR},
    result::{FileEntry, ForgeQLResult, ShowContent},
    session::Session,
};

use super::ForgeQLEngine;
use super::{DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, convert_show_json, reject_text_filter};

/// Extract the `backend` selector from any supported SHOW / `FindFiles` op.
///
/// Returns `Backend::Default` for any op that does not carry a backend field
/// (e.g. mutation ops, which should never be dispatched here).
const fn backend_for_show_op(op: &ForgeQLIR) -> &Backend {
    match op {
        ForgeQLIR::ShowContext { backend, .. }
        | ForgeQLIR::ShowSignature { backend, .. }
        | ForgeQLIR::ShowOutline { backend, .. }
        | ForgeQLIR::ShowMembers { backend, .. }
        | ForgeQLIR::ShowBody { backend, .. }
        | ForgeQLIR::ShowCallees { backend, .. }
        | ForgeQLIR::ShowLines { backend, .. }
        | ForgeQLIR::FindFiles { backend, .. } => backend,
        _ => &Backend::Default,
    }
}

impl ForgeQLEngine {
    #[allow(clippy::too_many_lines)]
    pub(super) fn exec_show(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let backend = backend_for_show_op(op);
        let (workspace, engine) = self.require_workspace_and_engine_for(session_id, backend)?;
        let root = workspace.root();

        let json = match op {
            ForgeQLIR::ShowContext {
                symbol, clauses, ..
            } => {
                let context_lines = clauses.depth.unwrap_or(DEFAULT_CONTEXT_LINES);
                engine
                    .resolve_symbol(symbol, clauses, root)
                    .and_then(|opt| {
                        opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found"))
                    })
                    .and_then(|loc| {
                        let bytes = crate::workspace::file_io::read_bytes(&loc.path)?;
                        show::show_context(
                            &bytes,
                            &loc.path,
                            loc.byte_range.start,
                            &workspace,
                            symbol,
                            context_lines,
                        )
                    })
                    .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
            }
            ForgeQLIR::ShowSignature {
                symbol, clauses, ..
            } => engine
                .resolve_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached =
                        self.get_or_parse_for_show(session_id, &loc.path, loc.blob_sha.as_ref())?;
                    show::show_signature(
                        &cached,
                        &loc.path,
                        loc.byte_range.start,
                        &loc.node_kind,
                        &workspace,
                        symbol,
                        &self.lang_registry,
                    )
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowOutline { file, .. } => engine
                .show_outline_for_file(&workspace, file)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowMembers {
                symbol, clauses, ..
            } => engine
                .resolve_type_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached =
                        self.get_or_parse_for_show(session_id, &loc.path, loc.blob_sha.as_ref())?;
                    show::show_members(&cached, &loc.path, &workspace, symbol, &self.lang_registry)
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowBody {
                symbol, clauses, ..
            } => engine
                .resolve_body_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached =
                        self.get_or_parse_for_show(session_id, &loc.path, loc.blob_sha.as_ref())?;
                    show::show_body(
                        &cached,
                        &loc.path,
                        loc.byte_range.start,
                        &loc.enrichment,
                        &workspace,
                        symbol,
                        Some(clauses.depth.unwrap_or(DEFAULT_BODY_DEPTH)),
                        &self.lang_registry,
                    )
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowCallees {
                symbol, clauses, ..
            } => engine
                .resolve_body_symbol(symbol, clauses, root)
                .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
                .and_then(|loc| {
                    let cached =
                        self.get_or_parse_for_show(session_id, &loc.path, loc.blob_sha.as_ref())?;
                    show::show_callees(
                        &cached,
                        &loc.path,
                        loc.byte_range.start,
                        &workspace,
                        symbol,
                        &self.lang_registry,
                        |name| engine.locate_definition(name),
                    )
                })
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => show::show_lines(&workspace, file, *start_line, *end_line)
                .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() })),
            ForgeQLIR::FindFiles { clauses, .. } => {
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

    /// Get a cached parse for the given path, or parse fresh on miss.
    ///
    /// Uses the session's `ParseCache` (capacity 32) when a session is active.
    /// Falls back to a one-shot parse when no session is available.
    ///
    /// `blob_sha` is the content SHA-1 known at resolve time (populated by
    /// the columnar backend from `SegmentMeta::content_id`).  When `Some`:
    /// - cache *hit* → returns immediately, no file read
    /// - cache *miss* → reads file but skips `sha1_of_bytes`
    fn get_or_parse_for_show(
        &self,
        session_id: Option<&str>,
        path: &Path,
        blob_sha: Option<&[u8; 20]>,
    ) -> Result<Arc<CachedParse>> {
        use crate::ast::parse_cache::ParseCache;

        if let Some(sid) = session_id
            && let Some(session) = self.sessions.get(sid)
        {
            let mut guard = session
                .parse_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return guard.get_or_parse_with_hint(path, &self.lang_registry, blob_sha);
        }
        // No active session — parse without cache.
        ParseCache::with_capacity(1).get_or_parse_with_hint(path, &self.lang_registry, blob_sha)
    }
}
