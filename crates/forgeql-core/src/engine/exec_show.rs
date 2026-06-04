use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    ast::{
        parse_cache::{CachedParse, sha1_of_bytes},
        query, show,
    },
    ir::{Backend, Clauses, ForgeQLIR, SortDirection},
    result::{FileEntry, ForgeQLResult, ShowContent},
    session::Session,
    storage::{StorageEngine, SymbolLocation},
    workspace::Workspace,
};

use super::ForgeQLEngine;
use super::{DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, convert_show_json, reject_text_filter};

/// Read the bytes for a symbol's source file, with a bare-repository fallback.
///
/// On normal working trees `file_io::read_bytes` succeeds.  On reconnected
/// bare clones (or detached worktrees where checked-out files are absent),
/// the regular read fails and we fall back to fetching the blob content
/// directly from git using the SHA-1 stored in `SymbolLocation::blob_sha`.
///
/// # Errors
/// - I/O error on a non-bare workspace.
/// - Bare workspace with no `blob_sha` available.
/// - Git object-store lookup failure.
fn read_bytes_for_show(workspace: &Workspace, location: &SymbolLocation) -> Result<Vec<u8>> {
    match crate::workspace::file_io::read_bytes(&location.path) {
        Ok(b) => Ok(b),
        Err(_) if workspace.is_bare() => {
            let sha = location.blob_sha.ok_or_else(|| {
                anyhow::anyhow!(
                    "file not on disk and no blob SHA available for '{}'",
                    location.path.display()
                )
            })?;
            workspace.read_blob_by_sha(&sha)
        }
        Err(e) => Err(e),
    }
}

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
    pub(super) fn exec_show(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let backend = backend_for_show_op(op);
        let (workspace, engine) = self.require_workspace_and_engine_for(session_id, backend)?;

        let json = match op {
            ForgeQLIR::ShowContext {
                symbol, clauses, ..
            } => Self::exec_show_context(&workspace, engine, symbol, clauses),
            ForgeQLIR::ShowSignature {
                symbol, clauses, ..
            } => self.exec_show_signature(session_id, &workspace, engine, symbol, clauses),
            ForgeQLIR::ShowOutline { file, .. } => {
                Self::exec_show_outline(&workspace, engine, file)
            }
            ForgeQLIR::ShowMembers {
                symbol, clauses, ..
            } => self.exec_show_members(session_id, &workspace, engine, symbol, clauses),
            ForgeQLIR::ShowBody {
                symbol, clauses, ..
            } => self.exec_show_body(session_id, &workspace, engine, symbol, clauses),
            ForgeQLIR::ShowCallees {
                symbol, clauses, ..
            } => self.exec_show_callees(session_id, &workspace, engine, symbol, clauses),
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => Self::exec_show_lines(&workspace, file, *start_line, *end_line),
            ForgeQLIR::FindFiles { clauses, .. } => {
                Self::exec_show_find_files(&workspace, engine, clauses)?
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
                // Default sort for callees is by call-site line (ascending) so
                // the output reflects call order.  If the user supplied an
                // explicit ORDER BY, respect it instead.
                if clauses.order_by.is_none() {
                    let mut effective = clauses.clone();
                    effective.order_by = Some(crate::ir::OrderBy {
                        field: "line".to_string(),
                        direction: crate::ir::SortDirection::Asc,
                    });
                    crate::filter::apply_clauses(entries, &effective);
                } else {
                    crate::filter::apply_clauses(entries, clauses);
                }
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

    /// Get a cached parse for a symbol location, with bare-repo fallback.
    ///
    /// Uses the session's `ParseCache` (capacity 32) when a session is active;
    /// falls back to a one-shot parse when no session is available.
    ///
    /// Reading strategy:
    /// - **Cache hit** (by `blob_sha`): returns immediately — no file or git read.
    /// - **Cache miss**: calls `read_bytes_for_show` which transparently falls back
    ///   to `Workspace::read_blob_by_sha` on bare repos where the file is absent.
    ///
    /// If `blob_sha` is `None`, bytes are read from disk and the SHA-1 is
    /// computed from the content (legacy backend behaviour).
    fn get_or_parse_for_show(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        loc: &SymbolLocation,
    ) -> Result<Arc<CachedParse>> {
        use crate::ast::parse_cache::ParseCache;

        if let Some(sid) = session_id
            && let Some(session) = self.sessions.get(sid)
        {
            let mut guard = session
                .parse_cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            // Fast path: cache hit by blob SHA — no I/O of any kind.
            if let Some(sha) = loc.blob_sha.as_ref()
                && let Some(hit) = guard.get(sha)
            {
                return Ok(hit);
            }

            // Miss (or no SHA hint): read bytes with bare-repo fallback, then parse.
            let bytes = read_bytes_for_show(workspace, loc)?;
            let hash = loc.blob_sha.unwrap_or_else(|| sha1_of_bytes(&bytes));
            return guard.get_or_parse_with_bytes(hash, &loc.path, bytes, &self.lang_registry);
        }

        // No active session — one-shot parse with bare-repo fallback.
        let bytes = read_bytes_for_show(workspace, loc)?;
        let hash = loc.blob_sha.unwrap_or_else(|| sha1_of_bytes(&bytes));
        ParseCache::with_capacity(1).get_or_parse_with_bytes(
            hash,
            &loc.path,
            bytes,
            &self.lang_registry,
        )
    }

    fn exec_show_context(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        let context_lines = clauses.depth.unwrap_or(DEFAULT_CONTEXT_LINES);
        engine
            .resolve_symbol(symbol, clauses, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
            .and_then(|loc| {
                let bytes = read_bytes_for_show(workspace, &loc)?;
                show::show_context(
                    &bytes,
                    &loc.path,
                    loc.byte_range.start,
                    workspace,
                    symbol,
                    context_lines,
                )
            })
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    fn exec_show_signature(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        engine
            .resolve_symbol(symbol, clauses, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
            .and_then(|loc| {
                let cached = self.get_or_parse_for_show(session_id, workspace, &loc)?;
                let req = show::ShowRequest {
                    cached: &cached,
                    path: &loc.path,
                    byte_range_start: loc.byte_range.start,
                    hint_line: Some(loc.line).filter(|&l| l > 0),
                    workspace,
                    symbol,
                    lang_registry: &self.lang_registry,
                    ordinal: None,
                };
                show::show_signature(&req, &loc.node_kind)
            })
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    fn exec_show_outline(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        file: &str,
    ) -> serde_json::Value {
        engine
            .show_outline_for_file(workspace, file)
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    fn exec_show_members(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        engine
            .resolve_type_symbol(symbol, clauses, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
            .and_then(|loc| {
                let cached = self.get_or_parse_for_show(session_id, workspace, &loc)?;
                let req = show::ShowRequest {
                    cached: &cached,
                    path: &loc.path,
                    byte_range_start: loc.byte_range.start,
                    hint_line: Some(loc.line).filter(|&l| l > 0),
                    workspace,
                    symbol,
                    lang_registry: &self.lang_registry,
                    ordinal: None,
                };
                show::show_members(&req)
            })
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    fn exec_show_body(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        engine
            .resolve_body_symbol(symbol, clauses, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
            .and_then(|loc| {
                let cached = self.get_or_parse_for_show(session_id, workspace, &loc)?;
                let req = show::ShowRequest {
                    cached: &cached,
                    path: &loc.path,
                    byte_range_start: loc.byte_range.start,
                    hint_line: Some(loc.line).filter(|&l| l > 0),
                    workspace,
                    symbol,
                    lang_registry: &self.lang_registry,
                    ordinal: loc.ordinal,
                };
                show::show_body(
                    &req,
                    Some(clauses.depth.unwrap_or(DEFAULT_BODY_DEPTH)),
                    &loc.enrichment,
                )
            })
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    fn exec_show_callees(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        engine
            .resolve_body_symbol(symbol, clauses, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| anyhow::anyhow!("symbol '{symbol}' not found")))
            .and_then(|loc| {
                let cached = self.get_or_parse_for_show(session_id, workspace, &loc)?;
                let req = show::ShowRequest {
                    cached: &cached,
                    path: &loc.path,
                    byte_range_start: loc.byte_range.start,
                    hint_line: Some(loc.line).filter(|&l| l > 0),
                    workspace,
                    symbol,
                    lang_registry: &self.lang_registry,
                    ordinal: None,
                };
                show::show_callees(&req)
            })
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    fn exec_show_lines(
        workspace: &Workspace,
        file: &str,
        start_line: usize,
        end_line: usize,
    ) -> serde_json::Value {
        show::show_lines(workspace, file, start_line, end_line)
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "FindFiles clause pipeline; splitting would scatter tightly-coupled logic"
    )]
    fn exec_show_find_files(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        clauses: &Clauses,
    ) -> Result<serde_json::Value> {
        reject_text_filter(clauses)?;
        let glob = clauses.in_glob.as_deref().unwrap_or("**");
        let indexed_opt = engine.indexed_files();
        let fast_path_ext: Option<&str> = indexed_opt.as_ref().and_then(|indexed| {
            use crate::ir::{CompareOp, PredicateValue};
            clauses
                .where_predicates
                .iter()
                .find_map(|p| {
                    if (p.field == "extension" || p.field == "ext")
                        && p.op == CompareOp::Eq
                        && let PredicateValue::String(s) = &p.value
                    {
                        return Some(s.as_str());
                    }
                    None
                })
                .filter(|ext| indexed.iter().any(|fe| fe.extension == *ext))
        });
        let mut entries: Vec<FileEntry> = if fast_path_ext.is_some() {
            #[expect(
                clippy::unwrap_used,
                reason = "fast_path_ext.is_some() implies indexed_opt.is_some() — invariant established above"
            )]
            indexed_opt.unwrap()
        } else {
            let raw = query::find_files(workspace, glob, clauses.exclude_glob.as_deref());
            raw.iter()
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
                    let depth = Some(path.components().count());
                    Some(FileEntry {
                        path,
                        extension,
                        size,
                        depth,
                        count: None,
                    })
                })
                .collect()
        };
        let max_depth = clauses.depth.unwrap_or(usize::MAX);
        let results: Vec<serde_json::Value> =
            if clauses.depth.is_some() && clauses.group_by.is_none() {
                let mut filter_clauses = clauses.clone();
                filter_clauses.order_by = None;
                filter_clauses.limit = None;
                filter_clauses.offset = None;
                crate::filter::apply_clauses(&mut entries, &filter_clauses);

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
                let mut grouped = query::group_files_by_depth(&file_json, max_depth);

                if let Some(ref order_by) = clauses.order_by {
                    let dir = order_by.direction;
                    let field = order_by.field.clone();
                    grouped.sort_by(|a, b| {
                        let cmp = if let (Some(va), Some(vb)) = (
                            a.get(&field).and_then(serde_json::Value::as_u64),
                            b.get(&field).and_then(serde_json::Value::as_u64),
                        ) {
                            va.cmp(&vb)
                        } else {
                            let sa = a
                                .get(&field)
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("");
                            let sb = b
                                .get(&field)
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("");
                            sa.cmp(sb)
                        };
                        match dir {
                            SortDirection::Desc => cmp.reverse(),
                            SortDirection::Asc => cmp,
                        }
                    });
                }

                let skip = clauses.offset.unwrap_or(0);
                if skip > 0 {
                    drop(grouped.drain(..skip.min(grouped.len())));
                }
                if let Some(max) = clauses.limit {
                    grouped.truncate(max);
                }
                grouped
            } else if clauses.group_by.is_some() {
                crate::filter::apply_clauses(&mut entries, clauses);
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
                crate::filter::apply_clauses(&mut entries, clauses);
                entries
                    .iter()
                    .map(|fe| {
                        serde_json::json!({
                            "path":      fe.path.display().to_string(),
                            "extension": fe.extension,
                            "size":      fe.size,
                        })
                    })
                    .collect()
            };
        let count = results.len();
        Ok(serde_json::json!({
            "op":      "find_files",
            "glob":    glob,
            "depth":   max_depth,
            "results": results,
            "count":   count,
        }))
    }
}
