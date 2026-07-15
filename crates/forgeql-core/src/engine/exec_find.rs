use anyhow::Result;

use crate::{
    ir::{Backend, Clauses, GroupBy},
    result::{ForgeQLResult, QueryResult, ShowContent, ShowResult},
    session::last_set::{self, LastMember, LastSet},
};

use super::ForgeQLEngine;
use super::{detect_metric_hint, reject_text_filter, require_session_id};
impl ForgeQLEngine {
    pub(super) fn find_symbols(
        &mut self,
        session_id: Option<&str>,
        backend: &Backend,
        clauses: &Clauses,
    ) -> Result<ForgeQLResult> {
        reject_text_filter(clauses)?;
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let root = session.worktree_path.clone();

        // Delegate all filtering, fast-path GROUP BY, ORDER BY, explicit LIMIT
        // to the storage engine.  The engine returns sorted/filtered results
        // WITHOUT the implicit DEFAULT_QUERY_LIMIT cap — that is applied below.
        // The columnar backend uses clauses.limit for early-exit in
        // materialize_all, so explicit LIMIT queries avoid a full segment scan.
        let mut results = session.engine_for(backend)?.find_symbols(clauses, &root)?;

        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(session.output_config().find_limit);
        }

        let metric_hint = detect_metric_hint(clauses);
        let group_by_field = match &clauses.group_by {
            Some(GroupBy::Field(f)) if f != "fql_kind" && f != "file" => Some(f.clone()),
            _ => None,
        };
        let hint = Self::unknown_where_field_hint(clauses, &results);
        let last_rev = self.record_last_set(sid, "find_symbols", &results, total, clauses);

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results,
            total,
            metric_hint,
            group_by_field,
            hint,
            last_rev,
        }))
    }

    /// A one-line hint when the result set is empty and a WHERE field is not
    /// a core field, not an enrichment field of any registered language, and
    /// not carried by any row — the classic silent-empty-match footgun.
    /// Static text keyed on the observed input; no inference.
    fn unknown_where_field_hint(
        clauses: &Clauses,
        results: &[crate::result::SymbolMatch],
    ) -> Option<String> {
        if !results.is_empty() {
            return None;
        }
        for pred in &clauses.where_predicates {
            let field = pred.field.as_str();
            if crate::filter::CORE_WHERE_FIELDS.contains(&field) {
                continue;
            }
            if !crate::storage::legacy::is_known_enrichment_field(field) {
                return Some(format!(
                    "no rows carry a field named '{field}' — unknown WHERE fields \
                 match nothing. Check the spelling against the core fields \
                 (name, fql_kind, path, line, usages, …) and the enrichment \
                 fields in the syntax reference."
                ));
            }
        }
        None
    }

    /// `FIND usages OF 'symbol' ...`
    pub(super) fn find_usages(
        &mut self,
        session_id: Option<&str>,
        of: &str,
        backend: &Backend,
        clauses: &Clauses,
    ) -> Result<ForgeQLResult> {
        reject_text_filter(clauses)?;
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let root = session.worktree_path.clone();

        let mut results = session
            .engine_for(backend)?
            .find_usages(of, clauses, &root)?;

        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(session.output_config().find_limit);
        }
        let last_rev = self.record_last_set(sid, "find_usages", &results, total, clauses);

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results,
            total,
            metric_hint: None,
            group_by_field: None,
            hint: None,
            last_rev,
        }))
    }

    /// Arm `LAST` from a symbol/usage FIND result.
    ///
    /// Every FIND replaces the set — and a FIND whose rows carry no location
    /// (a `GROUP BY` aggregate) clears it rather than leaving the previous one
    /// armed. A set that survives the query the agent believes replaced it is
    /// how `CHANGE NODES LAST` ends up sweeping code nobody looked at.
    ///
    /// `complete` is false when the FIND was truncated: the members are exactly
    /// the rows returned, so a capped result can still be inspected, but no
    /// master rev will be issued for it and every LAST verb refuses.
    fn record_last_set(
        &mut self,
        sid: &str,
        origin: &str,
        results: &[crate::result::SymbolMatch],
        total: usize,
        clauses: &Clauses,
    ) -> Option<String> {
        let root = self.sessions.get(sid)?.worktree_path.clone();

        // An aggregate is not a set of nodes. `GROUP BY` rows are counts — they
        // may even carry a stray handle from the group's first member — and a
        // set armed from them addresses nothing anyone asked for. Read it off
        // the query, not the row shape: the query is what the agent wrote.
        //
        // The row check then keeps out anything that carries no location at all.
        let addressable = |r: &crate::result::SymbolMatch| {
            r.path.is_some() && (r.node_id.is_some() || r.line.is_some_and(|l| l >= 1))
        };
        let members: Vec<LastMember> =
            if clauses.group_by.is_none() && results.iter().all(addressable) {
                results
                    .iter()
                    .filter_map(|r| {
                        let path = r.path.as_ref()?;
                        // Backends may return worktree-absolute paths; store
                        // worktree-relative so the sweep can resolve them safely.
                        let rel = path.strip_prefix(&root).unwrap_or(path);
                        Some(LastMember {
                            node_id: r.node_id.clone(),
                            path: rel.to_string_lossy().into_owned(),
                            line: r.line.filter(|l| *l >= 1),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
        self.arm_last_set(sid, origin, members, total, results.len())
    }

    /// Store the set, or clear it when there is nothing addressable to store.
    ///
    /// The one place `Session::last_set` is written, so the on-disk copy cannot
    /// drift from the in-memory one: a session outlives the server process, and
    /// a LAST that survives only in RAM is a LAST the next process silently
    /// loses.
    fn arm_last_set(
        &mut self,
        sid: &str,
        origin: &str,
        members: Vec<LastMember>,
        total: usize,
        returned: usize,
    ) -> Option<String> {
        let root = self.sessions.get(sid)?.worktree_path.clone();
        if members.is_empty() {
            if let Some(session) = self.sessions.get_mut(sid) {
                session.last_set = None;
            }
            last_set::clear(&root);
            return None;
        }

        // A truncated result gets no master rev: without one every LAST verb
        // refuses, which is the whole point — the rows beyond the cap were
        // never shown, and a set the agent did not see is not a set it chose.
        let complete = total == returned;
        let master_rev = if complete {
            self.master_rev_of(sid, &members).ok()
        } else {
            None
        };

        let set = LastSet {
            origin: origin.to_string(),
            complete,
            master_rev: master_rev.clone(),
            members,
        };
        if let Err(err) = last_set::save(&set, &root) {
            tracing::warn!(
                error = %err,
                "could not persist the LAST set; a server restart will lose it"
            );
        }
        if let Some(session) = self.sessions.get_mut(sid) {
            session.last_set = Some(set);
        }
        master_rev
    }

    /// `FIND files` — executed by the SHOW family (it renders a file listing),
    /// but it is a FIND, so it arms LAST like the other two.
    ///
    /// Only handle-carrying rows arm it. A `GROUP BY` aggregate row is a count,
    /// not a node: there is nothing for a bulk verb to address and nothing for
    /// the master rev to fingerprint, so such a result clears LAST instead of
    /// leaving the previous one in place.
    pub(super) fn exec_find_files(
        &mut self,
        session_id: Option<&str>,
        op: &crate::ir::ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let mut result = self.exec_show(session_id, op)?;

        // As on FIND symbols: an aggregate is a count, not a set of nodes.
        let aggregate = matches!(op, crate::ir::ForgeQLIR::FindFiles { clauses, .. }
            if clauses.group_by.is_some());

        let (members, total, returned) = match &result {
            ForgeQLResult::Show(ShowResult {
                content: ShowContent::FileList { files, total },
                ..
            }) if !aggregate && files.iter().all(|f| f.node_id.is_some()) => (
                files
                    .iter()
                    .map(|f| LastMember {
                        node_id: f.node_id.clone(),
                        path: f.path.to_string_lossy().into_owned(),
                        line: None,
                    })
                    .collect(),
                *total,
                files.len(),
            ),
            _ => (Vec::new(), 0, 0),
        };
        let last_rev = self.arm_last_set(sid, "find_files", members, total, returned);

        // FIND files renders through the SHOW family, whose result has no
        // `last_rev` column of its own — the master rev rides in the metadata
        // map so the CSV row reads the same as it does on FIND symbols/usages.
        if let (Some(rev), ForgeQLResult::Show(show)) = (last_rev, &mut result) {
            drop(
                show.metadata
                    .get_or_insert_with(serde_json::Map::new)
                    .insert("last_rev".to_string(), serde_json::Value::String(rev)),
            );
        }
        Ok(result)
    }

    /// FIND NODE id — resolve a `node_id` to its location, rev, and nav links.
    pub(super) fn find_node(
        &self,
        session_id: Option<&str>,
        node_id: &str,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let root = &session.worktree_path;
        match session
            .engine_for(&crate::ir::Backend::Default)?
            .find_node(node_id, root)?
        {
            Some(mut r) => {
                // Relativize path so it matches what other commands return.
                if let Ok(rel) = r.path.strip_prefix(root) {
                    r.path = rel.to_path_buf();
                }
                Ok(ForgeQLResult::FindNode(r))
            }
            None => anyhow::bail!(
                r#"{{"error":"node_not_found","node_id":"{node_id}","suggested_next":"SHOW outline OF file"}}"#
            ),
        }
    }

    // -------------------------------------------------------------------
    // Show-line cap helper
    // -------------------------------------------------------------------

    /// Apply source-line result-set bounds in one place.
    ///
    /// Must be called **after** WHERE predicates have been applied so that
    /// the counts reflect post-filter line totals.
    ///
    /// Caps applied in order:
    /// 1. Explicit `LIMIT` + `OFFSET` from the agent's clauses — bounds the
    ///    result *set*, not the inline output.
    /// 2. Budget critical cap — truncates to `critical_max_lines` when the
    ///    session budget is in critical state.
    ///
    /// The implicit inline-output cap is intentionally NOT applied here: over-
    /// cap output is windowed and buffered for `SHOW MORE` at the single CSV
    /// render boundary (`mcp.rs::finalize_csv`), so the agent always receives
    /// the first page plus a pageable buffer rather than a hard empty result.
    pub(super) fn apply_show_lines_cap(
        show_result: &mut ShowResult,
        clauses: Option<&Clauses>,
        budget_max: Option<usize>,
    ) {
        // Operates only on source-line outputs.
        let total = match &show_result.content {
            ShowContent::Lines { lines, .. } => lines.len(),
            _ => return,
        };

        // ---- Explicit LIMIT + OFFSET (bounds the result *set*) ----
        // The inline output cap is deliberately NOT applied here. Over-cap
        // output is windowed and buffered for `SHOW MORE` at the single CSV
        // render boundary (mcp.rs::finalize_csv).
        if let Some(clauses) = clauses
            && clauses.limit.is_some()
        {
            // Agent gave an explicit LIMIT — honour OFFSET + LIMIT.
            if let ShowContent::Lines { lines, .. } = &mut show_result.content {
                let offset = clauses.offset.unwrap_or(0);
                if offset > 0 && offset < total {
                    *lines = lines.split_off(offset);
                } else if offset >= total {
                    lines.clear();
                }
                let limit = clauses.limit.unwrap_or(total);
                if lines.len() > limit {
                    lines.truncate(limit);
                }
            }
        }

        // ---- Budget critical cap ----
        if let Some(max) = budget_max {
            let count = match &show_result.content {
                ShowContent::Lines { lines, .. } => lines.len(),
                _ => return,
            };
            if count > max {
                if let ShowContent::Lines { lines, .. } = &mut show_result.content {
                    lines.truncate(max);
                }
                show_result.hint = Some(format!(
                    "Budget critical: output capped to {max} lines \
                     (requested {count}).  Use FIND to narrow your search."
                ));
            }
        }
    }

    // ===================================================================
    // Code exposure — SHOW commands
}
