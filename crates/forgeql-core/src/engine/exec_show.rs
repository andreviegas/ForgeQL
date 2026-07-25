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
use super::{convert_show_json, reject_text_filter};

mod more;
mod read;
mod stamps;

use stamps::{stamp_error_counts, stamp_member_handles, stamp_path_handles};

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

        let json = self.dispatch_show_op(session_id, op, &workspace, engine)?;

        // Check for error responses.
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            bail!("{err}");
        }

        // Convert the JSON value to a typed ShowResult.
        let mut show_result = convert_show_json(op, &json)?;

        // Apply the full clause pipeline (WHERE, ORDER BY, LIMIT, OFFSET, …) to
        // structured list results: outline, members, and call graph entries.
        Self::apply_list_clauses(&mut show_result.content, op);

        // Extract clauses for ShowContent::Lines variants.
        let show_clauses: Option<&Clauses> = match op {
            ForgeQLIR::ShowBody { clauses, .. }
            | ForgeQLIR::ShowLines { clauses, .. }
            | ForgeQLIR::ShowContext { clauses, .. } => Some(clauses),
            _ => None,
        };

        // Apply WHERE predicates BEFORE the line caps so queries like
        // `SHOW body OF 'fn' WHERE text MATCHES 'TODO'` filter over the full
        // function body, not just the first N lines.
        if let (ShowContent::Lines { lines, .. }, Some(clauses)) =
            (&mut show_result.content, show_clauses)
        {
            for predicate in &clauses.where_predicates {
                let pred = predicate.clone();
                lines.retain(|line| crate::filter::eval_predicate(line, &pred));
            }
        }

        // Bound the result *set* (explicit LIMIT/OFFSET) and apply the budget
        // critical cap. The inline output cap is applied later, at the single
        // CSV render boundary (mcp.rs::finalize_csv), which windows + buffers
        // any over-cap output for `SHOW MORE`.
        let budget_max = session_id
            .and_then(|sid| self.sessions.get(sid))
            .and_then(Session::budget_critical_max_lines);
        Self::apply_show_lines_cap(&mut show_result, show_clauses, budget_max);

        Ok(ForgeQLResult::Show(show_result))
    }

    /// Dispatch a `SHOW`/`FIND files` op to the matching backend handler,
    /// returning the raw JSON value (an `{ "error": … }` object for non-show ops).
    fn dispatch_show_op(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
    ) -> Result<serde_json::Value> {
        let json = match op {
            ForgeQLIR::ShowContext {
                symbol, clauses, ..
            } => Self::exec_show_context(workspace, engine, symbol, clauses),
            ForgeQLIR::ShowSignature {
                symbol, clauses, ..
            } => self.exec_show_signature(session_id, workspace, engine, symbol, clauses),
            ForgeQLIR::ShowOutline {
                file, all, clauses, ..
            } => {
                // Default outline is structural-only. An explicit `ALL`, or a
                // `WHERE fql_kind = …` predicate, opts back into every node so
                // the post-hoc clause filter still has the full set to act on.
                let show_all = *all
                    || clauses
                        .where_predicates
                        .iter()
                        .any(|p| p.field == "fql_kind" || p.field == "node_kind");
                Self::exec_show_outline(workspace, engine, file, show_all)
            }
            ForgeQLIR::ShowMembers {
                symbol, clauses, ..
            } => self.exec_show_members(session_id, workspace, engine, symbol, clauses),
            ForgeQLIR::ShowBody {
                symbol, clauses, ..
            } => self.exec_show_body(session_id, workspace, engine, symbol, clauses),
            ForgeQLIR::ShowCallees {
                symbol, clauses, ..
            } => self.exec_show_callees(session_id, workspace, engine, symbol, clauses),
            ForgeQLIR::ShowLines {
                file,
                start_line,
                end_line,
                ..
            } => Self::exec_show_lines(workspace, engine, file, *start_line, *end_line),
            ForgeQLIR::FindFiles { clauses, .. } => {
                Self::exec_show_find_files(workspace, engine, clauses)?
            }
            other => serde_json::json!({ "error": format!("not a show op: {other:?}") }),
        };
        Ok(json)
    }

    /// Apply the clause pipeline to structured list results (outline / members /
    /// call graph).  Lines results are handled separately (cap-aware) by the caller.
    fn apply_list_clauses(content: &mut ShowContent, op: &ForgeQLIR) {
        match (content, op) {
            (ShowContent::Outline { entries }, ForgeQLIR::ShowOutline { clauses, .. }) => {
                crate::filter::apply_clauses_keep_order(entries, clauses);
            }
            (ShowContent::Members { members, .. }, ForgeQLIR::ShowMembers { clauses, .. }) => {
                crate::filter::apply_clauses(members, clauses);
            }
            (ShowContent::CallGraph { entries, .. }, ForgeQLIR::ShowCallees { clauses, .. }) => {
                // Default sort for callees is by call-site line (ascending) so
                // the output reflects call order.  An explicit ORDER BY wins.
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

    fn exec_show_outline(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        file: &str,
        all: bool,
    ) -> serde_json::Value {
        engine
            .show_outline_for_file(workspace, file, all)
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
                let mut json = show::show_members(&req)?;
                stamp_member_handles(engine, workspace, &loc.path, &mut json);
                Ok(json)
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
            // Files and directories in one list: a directory is an addressable
            // node too, and an agent that has to run a second query to see them
            // pays a round trip for nothing.
            let mut raw = query::find_files(workspace, glob, &clauses.exclude_globs);
            raw.extend(query::find_dirs(workspace, glob, &clauses.exclude_globs));
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
                        error_count: None,
                        parse_coverage: None,
                        node_id: None,
                        rev: None,
                    })
                })
                .collect()
        };
        let max_depth = clauses.depth.unwrap_or(usize::MAX);
        stamp_error_counts(engine, workspace.root(), clauses, &mut entries)?;

        // Rows that survive the filters but not the LIMIT. `count` alone cannot
        // tell a complete result from a capped one, and `LAST` has to know:
        // a set armed from rows the agent never saw is not a set it chose.
        let total = {
            let mut unbounded = clauses.clone();
            unbounded.limit = None;
            unbounded.offset = None;
            let mut probe = entries.clone();
            crate::filter::apply_clauses(&mut probe, &unbounded);
            probe.len()
        };

        let mut results = format_file_results(&mut entries, clauses, max_depth);
        stamp_path_handles(workspace, &mut results);
        let count = results.len();
        Ok(serde_json::json!({
            "op":      "find_files",
            "glob":    glob,
            "depth":   max_depth,
            "results": results,
            "count":   count,
            "total":   total,
        }))
    }
}

/// Base JSON object for a file entry: `path`, `extension`, `size`.
///
/// `node_id` and `rev` are stamped later, by `stamp_path_handles`, on the rows
/// that survive LIMIT.
fn file_entry_json(fe: &FileEntry) -> serde_json::Value {
    serde_json::json!({
        "path":      fe.path.display().to_string(),
        "extension": fe.extension,
        "size":      fe.size,
    })
}

/// Format file entries into result JSON, applying the clause-dependent shape:
/// depth-grouping (with its own sort/skip/limit), GROUP BY (carrying `count`),
/// or the plain filtered list.
fn format_file_results(
    entries: &mut Vec<FileEntry>,
    clauses: &Clauses,
    max_depth: usize,
) -> Vec<serde_json::Value> {
    if clauses.depth.is_some() && clauses.group_by.is_none() {
        // Depth grouping handles its own ordering/paging, so strip those here.
        let mut filter_clauses = clauses.clone();
        filter_clauses.order_by = None;
        filter_clauses.limit = None;
        filter_clauses.offset = None;
        crate::filter::apply_clauses(entries, &filter_clauses);

        let file_json: Vec<serde_json::Value> = entries.iter().map(file_entry_json).collect();
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
        crate::filter::apply_clauses(entries, clauses);
        entries
            .iter()
            .map(|fe| {
                let mut obj = file_entry_json(fe);
                if let Some(n) = fe.count {
                    obj["count"] = serde_json::Value::from(n);
                }
                obj
            })
            .collect()
    } else {
        crate::filter::apply_clauses(entries, clauses);
        entries.iter().map(file_entry_json).collect()
    }
}
