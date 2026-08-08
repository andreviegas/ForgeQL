//! The `SHOW` verbs that read source text: `SHOW context`, `SHOW signature`,
//! `SHOW body`, `SHOW LINES` and `SHOW NODE`.
//!
//! They all end in the same place — a span of lines out of one file — so they
//! share the line cap, the `WHERE text` predicate and the budget accounting
//! that the dispatcher applies on the way out.

use anyhow::{Result, bail};

use crate::{
    ast::show,
    engine::{DEFAULT_BODY_DEPTH, DEFAULT_CONTEXT_LINES, ForgeQLEngine, require_session_id},
    ir::{Backend, Clauses, ForgeQLIR},
    result::ForgeQLResult,
    storage::StorageEngine,
    workspace::Workspace,
};

use super::read_bytes_for_show;

impl ForgeQLEngine {
    pub(super) fn exec_show_context(
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        let context_lines = clauses.depth.unwrap_or(DEFAULT_CONTEXT_LINES);
        let lookup = crate::filter::clauses_for_lookup::<crate::result::SourceLine>(clauses);
        engine
            .resolve_symbol(symbol, &lookup, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| super::lookup_missed(symbol, &lookup)))
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

    pub(super) fn exec_show_signature(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        let lookup = crate::filter::clauses_for_lookup::<crate::result::SourceLine>(clauses);
        engine
            .resolve_symbol(symbol, &lookup, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| super::lookup_missed(symbol, &lookup)))
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

    pub(super) fn exec_show_body(
        &self,
        session_id: Option<&str>,
        workspace: &Workspace,
        engine: &dyn StorageEngine,
        symbol: &str,
        clauses: &Clauses,
    ) -> serde_json::Value {
        let lookup = crate::filter::clauses_for_lookup::<crate::result::SourceLine>(clauses);
        engine
            .resolve_body_symbol(symbol, &lookup, workspace.root())
            .and_then(|opt| opt.ok_or_else(|| super::lookup_missed(symbol, &lookup)))
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

    pub(super) fn exec_show_lines(
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
        // Resolve each node rev once (a node spans many lines) so SHOW output can
        // carry the IF REV a mutation needs; see SourceLine::rev.
        let mut rev_cache: std::collections::HashMap<String, Option<String>> =
            std::collections::HashMap::new();
        if let Some(arr) = json.get_mut("lines").and_then(|v| v.as_array_mut()) {
            for (line_obj, node_ref) in arr.iter_mut().zip(refs) {
                if let Some((node_id, node_start)) = node_ref {
                    let abs_line = line_obj
                        .get("line")
                        .and_then(serde_json::Value::as_u64)
                        .and_then(|n| usize::try_from(n).ok())
                        .unwrap_or(0);
                    let offset = abs_line.saturating_sub(node_start) + 1;
                    // Stamp the node rev so the read-then-edit flow (SHOW then
                    // CHANGE NODE id(off) IF REV) needs no second FIND. find_node is
                    // the canonical rev resolver — correct even for block-surfaced
                    // handles — and the per-node cache keeps a multi-line read cheap.
                    let rev = rev_cache
                        .entry(node_id.clone())
                        .or_insert_with(|| {
                            engine
                                .find_node(&node_id, workspace.root())
                                .ok()
                                .flatten()
                                .map(|n| n.rev)
                                .filter(|r| !r.is_empty())
                        })
                        .clone();
                    if let Some(rev) = rev {
                        line_obj["rev"] = serde_json::Value::String(rev);
                    }
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
    pub(in crate::engine) fn exec_show_node(
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

        // `SHOW NODE` never reaches `exec_show` as itself — CONTENT gets there
        // only by being re-synthesised as `ShowLines` below, and METADATA
        // returns before that — so the check has to run here or not at all.
        // Both forms address bytes by handle: nothing resolves a name, so the
        // whole clause can only be answered from the node's own lines.
        crate::filter::reject_unresolvable_fields::<crate::result::SourceLine>(
            "SHOW NODE",
            clauses,
        )?;

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
}
