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
    filter::{
        reject_refused_fields, reject_unresolvable_fields, reject_unresolvable_shaping_fields,
    },
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

/// The error when `SHOW … OF 'name'` addressed nothing.
///
/// The lookup half of the clause is part of the answer. "This name does not
/// exist" and "no candidate for this name satisfies the clause you wrote" are
/// two different facts, and an agent told only the first for the second will
/// go and check its spelling instead of its filter. Neither is a refusal —
/// every field named was one something could answer; they just excluded
/// everything between them.
pub(super) fn lookup_missed(symbol: &str, lookup: &Clauses) -> anyhow::Error {
    if lookup.where_predicates.is_empty() {
        anyhow::anyhow!("symbol '{symbol}' not found")
    } else {
        anyhow::anyhow!(
            "no symbol '{symbol}' matches {}",
            crate::filter::describe_predicates(&lookup.where_predicates)
        )
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

        // Before anything is looked up or read. A clause naming a field this
        // verb cannot answer is a fact about the query, and reporting it as
        // "no symbol matches" — which is what happens once a doomed predicate
        // reaches the lookup — would make it read as a fact about the code.
        Self::reject_show_clause_fields(op)?;

        let json = self.dispatch_show_op(session_id, op, &workspace, engine)?;

        // Check for error responses.
        if let Some(err) = json.get("error").and_then(|v| v.as_str()) {
            bail!("{err}");
        }

        // Convert the JSON value to a typed ShowResult.
        let mut show_result = convert_show_json(op, &json)?;

        // Apply the full clause pipeline (WHERE, ORDER BY, LIMIT, OFFSET, …) to
        // structured list results: outline, members, and call graph entries.
        // It cannot refuse — every field was checked before dispatch.
        Self::apply_list_clauses(&mut show_result.content, op);

        // Extract clauses for ShowContent::Lines variants. `SHOW signature` is
        // absent on purpose: it renders one line rather than a row set, so it
        // never produces `Lines` and has nothing here to filter — its clause is
        // refused or given to the lookup, in `reject_show_clause_fields`.
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
            // `SHOW LINES` names a file and `SHOW NODE` a handle — neither
            // resolves a name, and `exec_show_lines` is handed no clause at
            // all — so their whole clause belongs to these lines. `SHOW body`,
            // `SHOW context` and `SHOW signature` address a symbol, so their
            // `WHERE` was split before the lookup ran and only the row half
            // belongs here. Both were checked before dispatch.
            let row_clauses = if matches!(op, ForgeQLIR::ShowLines { .. }) {
                clauses.clone()
            } else {
                crate::filter::clauses_for_rows::<crate::result::SourceLine>(clauses)
            };

            for predicate in &row_clauses.where_predicates {
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

    /// Refuse a clause this verb cannot answer, before anything is looked up.
    ///
    /// Every SHOW verb that carries a clause is named here with the row shape
    /// it answers over, so that adding a verb without deciding how its fields
    /// are checked is a visible omission rather than a silent one. Which check
    /// applies depends on how many consumers the clause has:
    ///
    /// - One consumer — `SHOW outline`, `SHOW LINES` and `FIND files` name a
    ///   file or a path, so nothing resolves a symbol and the row shape is the
    ///   whole universe of legitimate names. Every clause is held to it.
    /// - Two consumers — `SHOW members`, `SHOW callees`, `SHOW body` and
    ///   `SHOW context` also address a symbol. Their `WHERE` is split between
    ///   the lookup and the rows, so it is checked only against the table's
    ///   refused set: what neither consumer can answer. `ORDER BY`, `GROUP BY`
    ///   and `HAVING` reach no lookup, so those are held to the row shape.
    /// - No rows at all — `SHOW signature` renders one line rather than a row
    ///   set, so its clause can only scope the lookup and a field that only a
    ///   line row carries is refused outright.
    ///
    /// The verbs absent from this list carry a clause too, and are checked
    /// where they execute, each before the work it gates: `SHOW NODE` in
    /// `exec_show::read` (it never reaches `exec_show` as itself — CONTENT
    /// arrives re-synthesised as `SHOW LINES`, and METADATA returns before
    /// that), `SHOW MORE` in `exec_show::more`, `SHOW COMMITS` in
    /// `exec_source::readouts`, `SHOW DIFF` in `exec_transaction`, and
    /// `FIND symbols`/`FIND usages` in the columnar backend's own Stage 0,
    /// which alone can see which enrichment columns a segment stored.
    fn reject_show_clause_fields(op: &ForgeQLIR) -> Result<()> {
        use crate::result::{CallGraphEntry, FileEntry, MemberEntry, OutlineEntry, SourceLine};

        match op {
            ForgeQLIR::ShowOutline { clauses, .. } => {
                reject_unresolvable_fields::<OutlineEntry>("SHOW outline", clauses)?;
                crate::filter::reject_depth("SHOW outline", clauses)
            }
            ForgeQLIR::FindFiles { clauses, .. } => {
                reject_unresolvable_fields::<FileEntry>("FIND files", clauses)
            }
            ForgeQLIR::ShowLines { clauses, .. } => {
                reject_unresolvable_fields::<SourceLine>("SHOW LINES", clauses)?;
                Self::reject_line_shaping("SHOW LINES", clauses)?;
                Self::reject_globs("SHOW LINES", clauses)?;
                crate::filter::reject_depth("SHOW LINES", clauses)
            }
            ForgeQLIR::ShowMembers { clauses, .. } => {
                reject_refused_fields::<MemberEntry>("SHOW members", clauses)?;
                reject_unresolvable_shaping_fields::<MemberEntry>("SHOW members", clauses)?;
                crate::filter::reject_depth("SHOW members", clauses)
            }
            ForgeQLIR::ShowCallees { clauses, .. } => {
                reject_refused_fields::<CallGraphEntry>("SHOW callees", clauses)?;
                reject_unresolvable_shaping_fields::<CallGraphEntry>("SHOW callees", clauses)?;
                crate::filter::reject_depth("SHOW callees", clauses)
            }
            ForgeQLIR::ShowBody { clauses, .. } | ForgeQLIR::ShowContext { clauses, .. } => {
                reject_refused_fields::<SourceLine>("a SHOW that reads lines", clauses)?;
                Self::reject_line_shaping("a SHOW that reads lines", clauses)
            }
            ForgeQLIR::ShowSignature { clauses, .. } => {
                reject_refused_fields::<SourceLine>("SHOW signature", clauses)?;
                Self::reject_signature_clause_fields(clauses)?;
                crate::filter::reject_depth("SHOW signature", clauses)
            }
            _ => Ok(()),
        }
    }

    /// Refuse `ORDER BY` / `GROUP BY` / `HAVING` on a verb that answers with
    /// source lines.
    ///
    /// Nothing sorts, groups or aggregates a line result: the pipeline between
    /// the `WHERE` filter and the line caps applies none of them. They were
    /// accepted anyway, and because `LIMIT` *is* honoured the silence produced
    /// a wrong answer rather than an inert one — `SHOW body OF 'f' DEPTH 99
    /// ORDER BY line DESC LIMIT 4` handed back the first four lines, the
    /// opposite page to the one asked for.
    ///
    /// Refusing is the mechanical answer: the engine applies a clause or says it
    /// cannot. Filtering a line result still works, because `WHERE` is applied.
    pub(in crate::engine) fn reject_line_shaping(verb: &str, clauses: &Clauses) -> Result<()> {
        if clauses.order_by.is_some()
            || clauses.group_by.is_some()
            || !clauses.having_predicates.is_empty()
        {
            bail!(
                "ORDER BY, GROUP BY and HAVING cannot be answered on {verb}: it answers with \
                 source lines in source order, so there is nothing here to shape. Filter them \
                 with WHERE, or use FIND symbols, whose rows sort and group."
            );
        }
        Ok(())
    }
    /// Refuse a `SHOW signature` clause that has nothing to act on.
    ///
    /// A signature is one rendered line, not a row set. There is no row half at
    /// all here: the whole clause goes to the lookup, so `ORDER BY`, `GROUP BY`
    /// and `HAVING` have nothing to shape, and a `WHERE` naming a field a line
    /// row carries and a symbol row does not — `text`, `marker`, `rev` — has
    /// nothing to filter. Saying so beats accepting them and answering as
    /// though they had been applied.
    ///
    /// The `WHERE` set is derived rather than listed: what a source line
    /// resolves, minus what a symbol row resolves. A name on both, such as
    /// `line` or `node_id`, is a name the lookup can use, and reaches it —
    /// which is why this verb hands the resolver its clause whole rather than
    /// the split half, whose row side would be a phantom that silently
    /// swallowed exactly those two names.
    fn reject_signature_clause_fields(clauses: &Clauses) -> Result<()> {
        use crate::filter::ClauseTarget as _;
        use crate::result::{SourceLine, SymbolMatch};

        Self::reject_line_shaping("SHOW signature", clauses)?;
        for pred in &clauses.where_predicates {
            let field = crate::field_tiers::canonical(&pred.field);
            let on_a_line =
                SourceLine::STR_FIELDS.contains(&field) || SourceLine::NUM_FIELDS.contains(&field);
            let on_a_symbol = SymbolMatch::STR_FIELDS.contains(&field)
                || SymbolMatch::NUM_FIELDS.contains(&field);
            if on_a_line && !on_a_symbol {
                bail!(
                    "WHERE {} cannot be answered on SHOW signature: it renders one line rather \
                     than a row set, so there is nothing here to filter. Use SHOW body OF, whose \
                     rows are source lines.",
                    pred.field
                );
            }
        }
        Ok(())
    }

    /// Refuse `IN` / `EXCLUDE` on a verb that resolves no name and whose rows
    /// carry no path.
    ///
    /// A glob is a statement about a file. `SHOW LINES` and `SHOW NODE` were
    /// already given the file — by path or by handle — and `SHOW MORE` pages a
    /// buffer whose lines came from wherever the earlier command looked. So
    /// there is no lookup for a glob to scope and no row path for it to match,
    /// and nothing in the pipeline reads it: it was accepted and wholly inert.
    pub(in crate::engine) fn reject_globs(verb: &str, clauses: &Clauses) -> Result<()> {
        if clauses.in_glob.is_some() || !clauses.exclude_globs.is_empty() {
            bail!(
                "IN / EXCLUDE cannot be answered on {verb}: it addresses lines that are already \
                 located, so a glob has no lookup to scope and no row path to match."
            );
        }
        Ok(())
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
    fn apply_list_clauses(content: &mut ShowContent, op: &ForgeQLIR) {
        match (content, op) {
            (ShowContent::Outline { entries }, ForgeQLIR::ShowOutline { clauses, .. }) => {
                // The fields were checked before dispatch; this only filters.
                crate::filter::apply_clauses_keep_order(entries, clauses);
            }
            (ShowContent::Members { members, .. }, ForgeQLIR::ShowMembers { clauses, .. }) => {
                // Only the row half: the lookup already took the predicates a
                // members row cannot answer, so re-applying them here is what
                // emptied the answer for a symbol that exists.
                let row_clauses =
                    crate::filter::clauses_for_rows::<crate::result::MemberEntry>(clauses);
                crate::filter::apply_clauses(members, &row_clauses);
            }
            (ShowContent::CallGraph { entries, .. }, ForgeQLIR::ShowCallees { clauses, .. }) => {
                let mut row_clauses =
                    crate::filter::clauses_for_rows::<crate::result::CallGraphEntry>(clauses);
                // Default sort for callees is by call-site line (ascending) so
                // the output reflects call order.  An explicit ORDER BY wins.
                if row_clauses.order_by.is_none() {
                    row_clauses.order_by = Some(crate::ir::OrderBy {
                        field: "line".to_string(),
                        direction: crate::ir::SortDirection::Asc,
                    });
                }
                crate::filter::apply_clauses(entries, &row_clauses);
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
}
