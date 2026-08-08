//! `SHOW` execution: one dispatcher, and one module per family of verb.
//!
//! - `read` — the verbs that quote source text: context, signature, body,
//!   `SHOW LINES` and `SHOW NODE`
//! - `listings` — the verbs that build rows instead: outline, members, callees
//!   and `FIND files`
//! - `more` — `SHOW MORE`, paging the window a capped read left behind
//! - `stamps` — the handle and error-count stamping applied to finished rows
//!
//! What stays here is what every verb shares: the dispatcher, the clause
//! pipeline applied on the way out, and the readers that fetch a symbol's
//! bytes.

mod listings;
mod more;
mod read;
mod stamps;

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    ast::parse_cache::{CachedParse, sha1_of_bytes},
    filter::{reject_refused_fields, reject_unresolvable_fields},
    ir::{Backend, Clauses, ForgeQLIR},
    result::{ForgeQLResult, ShowContent},
    session::Session,
    storage::{StorageEngine, SymbolLocation},
    workspace::Workspace,
};

use super::ForgeQLEngine;
use super::convert_show_json;

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
        // It can refuse: a clause naming a field the row shape cannot carry is
        // an error, not an empty result.
        Self::apply_list_clauses(&mut show_result.content, op)?;

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
        //
        // Only the table check here, not the row-shape one: this clause was
        // also handed to the symbol lookup, so a source line's own field list
        // is not the universe of legitimate names. `reject_refused_fields`
        // documents the defect that leaves standing — on the columnar backend
        // the lookup evaluates none of these predicates, so a disambiguating
        // `WHERE language = '…'` scopes nothing and then filters away every
        // line of the body it did find.
        if let (ShowContent::Lines { lines, .. }, Some(clauses)) =
            (&mut show_result.content, show_clauses)
        {
            reject_refused_fields::<crate::result::SourceLine>("a SHOW that reads lines", clauses)?;
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
                // A bare outline lists structural declarations only; `ALL` —
                // or ANY `WHERE` — opens every node so the post-hoc clause
                // filter has the full set to act on.
                //
                // Any, not just a kind predicate. Which field the predicate
                // named used to decide the universe, so `WHERE fql_kind =
                // 'guard'` searched the file and `WHERE name = 'x'` searched
                // only the structural tree — two predicates on one verb,
                // answering over different row sets, and reporting different
                // `depth` for the same node because depth counts the ancestors
                // that were listed. A filter that cannot see a row cannot
                // report it, which is a false absence however the row set is
                // explained.
                let show_all = *all || !clauses.where_predicates.is_empty();
                Self::exec_show_outline(workspace, engine, file, show_all)?
            }
            ForgeQLIR::ShowMembers {
                symbol, clauses, ..
            } => self.exec_show_members(session_id, workspace, engine, symbol, clauses)?,
            ForgeQLIR::ShowBody {
                symbol, clauses, ..
            } => self.exec_show_body(session_id, workspace, engine, symbol, clauses),
            ForgeQLIR::ShowCallees {
                symbol, clauses, ..
            } => self.exec_show_callees(session_id, workspace, engine, symbol, clauses)?,
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
    fn apply_list_clauses(content: &mut ShowContent, op: &ForgeQLIR) -> Result<()> {
        match (content, op) {
            (ShowContent::Outline { entries }, ForgeQLIR::ShowOutline { clauses, .. }) => {
                // The one listing verb whose clause only filters: `SHOW outline
                // OF` names a file, so nothing here resolves a symbol and the
                // row's own field list is the whole universe of legitimate
                // names.
                reject_unresolvable_fields::<crate::result::OutlineEntry>("SHOW outline", clauses)?;
                crate::filter::apply_clauses_keep_order(entries, clauses);
            }
            (ShowContent::Members { members, .. }, ForgeQLIR::ShowMembers { clauses, .. }) => {
                // Only the table check: this clause was also handed to the
                // symbol lookup, so the row's own field list is not the
                // universe of legitimate names. See `reject_refused_fields`
                // for the defect that leaves standing.
                reject_refused_fields::<crate::result::MemberEntry>("SHOW members", clauses)?;
                crate::filter::apply_clauses(members, clauses);
            }
            (ShowContent::CallGraph { entries, .. }, ForgeQLIR::ShowCallees { clauses, .. }) => {
                // Same dual purpose as `SHOW members`, same check.
                reject_refused_fields::<crate::result::CallGraphEntry>("SHOW callees", clauses)?;
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
        Ok(())
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
}
