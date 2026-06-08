use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    ast::{
        parse_cache::{CachedParse, sha1_of_bytes},
        query, show,
    },
    ir::{Backend, Clauses, ForgeQLIR, SortDirection},
    result::{FileEntry, ForgeQLResult, ShowContent, ShowResult, SourceLine},
    session::Session,
    storage::{StorageEngine, SymbolLocation},
    workspace::Workspace,
};

use super::ForgeQLEngine;
use super::{
    DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, convert_show_json, reject_text_filter,
    require_session_id,
};

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
                Self::exec_show_outline(&workspace, engine, file, show_all)
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
            } => Self::exec_show_lines(&workspace, engine, file, *start_line, *end_line),
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
                crate::filter::apply_clauses_keep_order(entries, clauses);
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

        // The implicit 40-line cap blocks only UNBOUNDED source output. Two
        // shapes are bounded and must never be blocked:
        //   - SHOW LINES n-m       — a user-specified line range.
        //   - SHOW body OF 'sym'   — a single addressable symbol's own extent.
        // (SHOW NODE CONTENT is rewritten into a SHOW LINES range above, so it
        // is already covered.) SHOW context can expand via DEPTH, so it stays
        // capped.
        let is_explicit_range =
            matches!(op, ForgeQLIR::ShowLines { .. } | ForgeQLIR::ShowBody { .. });

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
        let show_limit = session_id
            .and_then(|sid| self.sessions.get(sid))
            .map_or_else(
                || crate::config::OutputConfig::default().show_lines,
                |s| s.output_config().show_lines,
            );
        Self::apply_show_lines_cap(
            &mut show_result,
            show_clauses,
            budget_max,
            is_explicit_range,
            show_limit,
        );

        Ok(ForgeQLResult::Show(show_result))
    }

    /// `SHOW MORE [HEAD n | TAIL n | n-m] [WHERE …] [LIMIT n]`
    ///
    /// Pages the session's last buffered output (`.forgeql-showmore`). The
    /// positional window is applied first, then `WHERE text` predicates and
    /// `LIMIT`/`OFFSET` reuse the same line machinery as `SHOW LINES`. Each
    /// returned line keeps its original buffer index so a precise follow-up
    /// range can be requested. `SHOW MORE` is an explicit retrieval and is
    /// never re-blocked by the inline cap.
    pub(super) fn exec_show_more(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let ForgeQLIR::ShowMore { window, clauses } = op else {
            unreachable!("exec_show_more: wrong IR variant")
        };
        let sid = require_session_id(session_id)?;
        let root = self.require_session(sid)?.worktree_path.clone();

        let buffer = crate::showmore::read_buffer(&root)
            .map_err(|e| anyhow::anyhow!("reading SHOW MORE buffer: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no SHOW MORE buffer for this session yet — run a command whose \
                     output was truncated, then SHOW MORE to page the rest"
                )
            })?;

        let selection = match *window {
            crate::ir::ShowMoreWindow::Full => crate::showmore::Selection::Full,
            crate::ir::ShowMoreWindow::Head(n) => crate::showmore::Selection::Head(n),
            crate::ir::ShowMoreWindow::Tail(n) => crate::showmore::Selection::Tail(n),
            crate::ir::ShowMoreWindow::Range(a, b) => crate::showmore::Selection::Range(a, b),
        };

        let total = buffer.total();
        let mut lines: Vec<SourceLine> = buffer
            .window(selection)
            .into_iter()
            .map(|(idx, text)| SourceLine {
                line: idx,
                text: text.to_string(),
                marker: None,
                node_id: None,
                node_offset: None,
            })
            .collect();

        // WHERE text predicates filter the windowed lines — free grep over the
        // buffered output (e.g. `SHOW MORE WHERE text MATCHES 'error|fail'`).
        for predicate in &clauses.where_predicates {
            let pred = predicate.clone();
            lines.retain(|line| crate::filter::eval_predicate(line, &pred));
        }

        let mut show_result = ShowResult {
            op: "show_more".to_string(),
            symbol: Some(buffer.label.clone()),
            file: None,
            content: ShowContent::Lines {
                lines,
                byte_start: None,
                depth: None,
            },
            start_line: None,
            end_line: None,
            total_lines: None,
            hint: None,
            metadata: None,
        };

        // Honour an explicit LIMIT/OFFSET; is_explicit_range = true so the
        // inline cap never blocks an explicit SHOW MORE retrieval.
        Self::apply_show_lines_cap(&mut show_result, Some(clauses), None, true, usize::MAX);

        // Tell the agent how much of the buffer it is seeing, so it can page on.
        let shown = match &show_result.content {
            ShowContent::Lines { lines, .. } => lines.len(),
            _ => 0,
        };
        if shown < total {
            show_result.total_lines = Some(total);
            show_result.hint = Some(format!(
                "buffer '{}' has {total} lines; showing {shown}. \
                 Page with SHOW MORE HEAD n | TAIL n | n-m, or filter with \
                 SHOW MORE WHERE text MATCHES '…'.",
                buffer.label
            ));
        }

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
        engine: &dyn StorageEngine,
        file: &str,
        start_line: usize,
        end_line: usize,
    ) -> serde_json::Value {
        let mut json = show::show_lines(workspace, file, start_line, end_line)
            .unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }));
        if json.get("error").is_some() {
            return json;
        }
        // Annotate each line with the innermost addressable node that contains
        // it (+ a 1-based node-relative offset) so SHOW LINES renders node
        // handles and offsets instead of absolute line numbers on parsed files.
        // An empty result (unparsed file or stale index) leaves the lines with
        // their absolute numbers untouched.
        let Ok(abs) = workspace.safe_path(file) else {
            return json;
        };
        let rel = workspace.relative(&abs);
        let rel_str = rel.to_string_lossy();
        let lo = json
            .get("start_line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(start_line);
        let hi = json
            .get("end_line")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(end_line);
        let refs = engine.innermost_nodes_for_lines(&rel_str, workspace.root(), lo, hi);
        if refs.is_empty() {
            return json;
        }
        if let Some(arr) = json.get_mut("lines").and_then(|v| v.as_array_mut()) {
            for (line_obj, node_ref) in arr.iter_mut().zip(refs) {
                if let Some((node_id, node_start)) = node_ref {
                    let abs_line = line_obj
                        .get("line")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|n| usize::try_from(n).ok())
                        .unwrap_or(0);
                    let offset = abs_line.saturating_sub(node_start) + 1;
                    line_obj["node_id"] = serde_json::Value::String(node_id);
                    line_obj["offset"] = serde_json::json!(offset);
                }
            }
        }
        json
    }

    /// `SHOW NODE 'id' [CONTENT | METADATA]`
    ///
    /// `id` may carry a node-relative line offset suffix — `id(n)` for a single
    /// line or `id(n-m)` for an inclusive range, both 1-based within the node's
    /// own span. The offset narrows CONTENT; it is rejected with METADATA.
    ///
    /// Resolves the base `node_id` to its current location, then either:
    /// - **CONTENT** (default): delegates to `exec_show(ShowLines)` so all
    ///   line-cap, WHERE-predicate, and budget logic is reused unchanged.
    /// - **METADATA**: returns `ForgeQLResult::FindNode` (same as `FIND NODE`).
    pub(super) fn exec_show_node(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let ForgeQLIR::ShowNode {
            node_id,
            metadata,
            clauses,
        } = op
        else {
            unreachable!("exec_show_node: wrong IR variant")
        };
        let sid = require_session_id(session_id)?;

        // A node_id may carry a node-relative line offset suffix — `id(n)` or
        // `id(n-m)`. METADATA describes the whole node, so an offset is only
        // meaningful for CONTENT; resolve the base node either way.
        let (base_id, offset) =
            crate::node_id::split_node_offset(node_id).map_err(|e| anyhow::anyhow!(e))?;

        // Resolve node_id in a block so the session borrow drops before exec_show.
        let node = {
            let session = self.require_session(sid)?;
            let root = session.worktree_path.clone();
            let mut r = session
                .engine_for(&Backend::Default)?
                .find_node(base_id, &root)?
                .ok_or_else(|| {
                    anyhow::anyhow!(r#"{{"error":"node_not_found","node_id":"{base_id}"}}"#)
                })?;
            if let Ok(rel) = r.path.strip_prefix(&root) {
                r.path = rel.to_path_buf();
            }
            r
        };

        if *metadata {
            if offset.is_some() {
                bail!("line offset is not supported with METADATA; it applies to CONTENT only");
            }
            return Ok(ForgeQLResult::FindNode(node));
        }

        // CONTENT: synthesize a ShowLines IR and delegate — reuses all caps/budget/WHERE.
        // A node-relative offset narrows the range inside the node's own span.
        let (start_line, end_line) = crate::node_id::offset_lines(node.line, node.end_line, offset)
            .map_err(|e| anyhow::anyhow!(e))?;
        let show_op = ForgeQLIR::ShowLines {
            file: node.path.to_string_lossy().into_owned(),
            start_line,
            end_line,
            backend: Backend::Default,
            clauses: clauses.clone(),
        };
        self.exec_show(session_id, &show_op)
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
