use anyhow::Result;

use crate::error::{ForgeError, RejectionKind};
use crate::{
    ir::{Backend, Clauses, GroupBy},
    result::{ForgeQLResult, QueryResult, ShowContent, ShowResult},
    session::found_set::{self, FoundMember, FoundSet},
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
        // That early exit is not free: with no ORDER BY it truncates the scan
        // rather than paging it, so the `total` taken below is the count of rows
        // FETCHED, not of rows that matched.  It is why the budget refusal no
        // longer offers a bare LIMIT as a way to bound an oversized scan.
        let mut results = session.engine_for(backend)?.find_symbols(clauses, &root)?;

        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(session.output_config().find_limit);
        }

        let metric_hint = detect_metric_hint(clauses);
        // `fql_kind` is the one grouping the compact renderer has its own
        // layout for, so it is deliberately NOT named here. Canonical, because
        // `GROUP BY kind` is the same grouping: naming it here sent the alias
        // down the generic layout and rendered `"function",3` where `fql_kind`
        // rendered `"function","[function,,0,3]"` — the same groups, two
        // different answers.
        let group_by_field = match &clauses.group_by {
            Some(GroupBy::Field(f)) if crate::field_tiers::canonical(f) != "fql_kind" => {
                Some(f.clone())
            }
            _ => None,
        };
        // A misspelled field and a name that only exists as a usage are
        // different problems; the field one comes first because it explains
        // an empty set that has nothing to do with the code.
        let hint = Self::unknown_where_field_hint(clauses, &results).or_else(|| {
            Self::zero_symbols_with_usages_hint(
                session.engine_for(backend).ok()?,
                clauses,
                &results,
                &root,
            )
        });
        let found_rev = self.record_found_set(sid, "find_symbols", &results, total, clauses);

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results,
            total,
            metric_hint,
            group_by_field,
            hint,
            found_rev,
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

    /// When a name query finds nothing but the index knows the name as a
    /// usage, say where to look instead of leaving "not here" as the answer.
    ///
    /// Empty is a real answer for a typo and a misleading one for a name the
    /// worktree only ever *references* — a macro defined in a header outside
    /// it, a symbol reached through an include that is not indexed. Both look
    /// identical in a `FIND symbols` response, and an agent that reads the
    /// empty set as "absent" abandons a rename that had work to do.
    ///
    /// Only runs on an empty result, so the extra postings lookup costs
    /// nothing on the path that found something.
    fn zero_symbols_with_usages_hint(
        engine: &dyn crate::storage::StorageEngine,
        clauses: &Clauses,
        results: &[crate::result::SymbolMatch],
        root: &std::path::Path,
    ) -> Option<String> {
        use crate::ir::{CompareOp, PredicateValue};
        if !results.is_empty() {
            return None;
        }
        // Only an exact name asks a question `FIND usages OF` can answer; a
        // LIKE pattern is not a name.
        let name = clauses.where_predicates.iter().find_map(|p| {
            match (p.field.as_str(), &p.op, &p.value) {
                ("name", CompareOp::Eq, PredicateValue::String(s)) => Some(s.clone()),
                _ => None,
            }
        })?;
        let sites = engine
            .find_usages(&name, &Clauses::default(), root)
            .ok()?
            .0
            .len();
        if sites == 0 {
            return None;
        }
        Some(format!(
            "0 symbols, but {sites} usage site(s) carry that name — it is \
             referenced here without being declared here. \
             FIND usages OF '{name}'"
        ))
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
        let find_limit = session.output_config().find_limit;

        // A usage site is one line of one file, and the question behind the
        // query is "which files hold this name?".  So the cap counts files, not
        // rows: the backend is asked for every site — LIMIT and OFFSET are
        // withheld from it — and whole file groups are selected below.  Cutting
        // the site list at a row count instead would report a file as partly
        // used and drop the rest of it with no marker.
        //
        // GROUP BY is exempt.  Its rows are aggregates, already one per group,
        // and its own LIMIT counts those groups; re-grouping them by path would
        // collapse a `GROUP BY` on any other field into a single bucket.
        let grouped = clauses.group_by.is_some();
        let mut engine_clauses = clauses.clone();
        if !grouped {
            engine_clauses.limit = None;
            engine_clauses.offset = None;
        }

        let (mut results, verify_hint) =
            session
                .engine_for(backend)?
                .find_usages(of, &engine_clauses, &root)?;

        // `total` is the true site count even under an explicit LIMIT — the
        // number a rename campaign measures its progress against.  FIND symbols
        // still reports a LIMIT-capped total; the divergence is deliberate.
        let total = results.len();
        let mut withheld = None;
        if grouped {
            if clauses.limit.is_none() {
                results.truncate(find_limit);
            }
        } else {
            let selected = crate::filter::take_file_groups(
                results,
                clauses.offset.unwrap_or(0),
                clauses.limit.unwrap_or(find_limit),
                crate::filter::USAGE_SITE_CEILING,
            );
            results = selected.rows;
            withheld = selected.withheld;
        }
        let found_rev = self.record_found_set(sid, "find_usages", &results, total, clauses);

        // A site row gains the handle and rev of the file it sits in, so an
        // agent can write `CHANGE NODE '<file>(<line>)' IF REV …` straight off
        // the row.  Two constraints on where this may happen:
        //
        // A FOUND member is routed by its handle when it has one, and a file
        // handle resolves to the file's WHOLE span — a stamped member would
        // turn `CHANGE NODES FOUND` into a whole-file sweep.  `record_found_set`
        // refuses a handle for this origin, and stamping runs after it anyway.
        //
        // Aggregate rows are skipped: `apply_group_by` keeps one arbitrary
        // member per group, so stamping a `GROUP BY` row would hand back the
        // handle of one file for a count spanning many — addressability the row
        // does not have.
        if !grouped {
            Self::stamp_file_handles(&root, &mut results);
        }
        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results,
            total,
            metric_hint: None,
            group_by_field: match &clauses.group_by {
                Some(GroupBy::Field(f)) if crate::field_tiers::canonical(f) != "fql_kind" => {
                    Some(f.clone())
                }
                _ => None,
            },
            hint: Self::withheld_hint(withheld)
                .into_iter()
                .chain(verify_hint)
                .reduce(|a, b| format!("{a} {b}")),
            found_rev,
        }))
    }

    /// One line telling the agent that files it did not see hold sites too.
    ///
    /// The two causes need different advice, and only one of them is fixed by
    /// asking for more files: past the site ceiling a bigger `LIMIT` changes
    /// nothing, because the cap is on sites rendered, not files requested.
    fn withheld_hint(withheld: Option<crate::filter::Withheld>) -> Option<String> {
        use crate::filter::Withheld;
        match withheld? {
            Withheld::Limit => Some(
                "more files hold sites than this listing shows — raise the \
                 LIMIT, or narrow with IN / WHERE. `total` is the true site \
                 count across all of them"
                    .to_string(),
            ),
            Withheld::Ceiling => Some(
                "files withheld to bound the response: the files shown already \
                 hold as many sites as one listing renders. Page the rest with \
                 OFFSET past the files shown — a larger LIMIT will not add \
                 files here — or narrow with IN / WHERE, or use GROUP BY file \
                 for per-file counts without the line lists"
                    .to_string(),
            ),
        }
    }

    /// Give every usage row the handle and rev of the file it sits in.
    ///
    /// A site is a line, not a node, so it has no handle of its own — but the
    /// file it lives in does, and that is enough to act on it:
    /// `CHANGE NODE '<file_hex>(<line>)' IF REV '<file rev>' MATCHING WORD …`
    /// edits exactly that line, rev-gated. The rev is the file's, so it is
    /// coarse on purpose: any edit anywhere in the file invalidates it, which
    /// is the honest gate for a target addressed by line number.
    ///
    /// One read per file, not per row: a campaign query returns many sites in
    /// the same file, and after file-group capping the file count is bounded
    /// by the limit.
    fn stamp_file_handles(root: &std::path::Path, results: &mut [crate::result::SymbolMatch]) {
        let mut seen: std::collections::HashMap<std::path::PathBuf, (String, String)> =
            std::collections::HashMap::new();
        for row in results.iter_mut() {
            let Some(path) = row.path.clone() else {
                continue;
            };
            // Backends may hand back worktree-absolute paths; the handle is
            // minted from the relative one, exactly as `FIND files` does.
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let (handle, rev) = seen.entry(rel.clone()).or_insert_with(|| {
                (
                    crate::node_id::path_handle(&rel.to_string_lossy()),
                    crate::node_id::file_rev(&root.join(&rel)),
                )
            });
            row.node_id = Some(handle.clone());
            if !rev.is_empty() {
                row.rev = Some(rev.clone());
            }
        }
    }

    /// Arm `FOUND` from a symbol/usage FIND result.
    ///
    /// Every FIND replaces the set — and a FIND whose rows carry no location
    /// (a `GROUP BY` aggregate) clears it rather than leaving the previous one
    /// armed. A set that survives the query the agent believes replaced it is
    /// how `CHANGE NODES FOUND` ends up sweeping code nobody looked at.
    ///
    /// `complete` is false when the FIND was truncated: the members are exactly
    /// the rows returned, so a capped result can still be inspected, but no
    /// master rev will be issued for it and every FOUND verb refuses.
    fn record_found_set(
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
        let members: Vec<FoundMember> =
            if clauses.group_by.is_none() && results.iter().all(addressable) {
                results
                    .iter()
                    .filter_map(|r| {
                        let path = r.path.as_ref()?;
                        // Backends may return worktree-absolute paths; store
                        // worktree-relative so the sweep can resolve them safely.
                        let rel = path.strip_prefix(&root).unwrap_or(path);
                        Some(FoundMember {
                            // A usage site is a line, not a node. Its row does
                            // display the handle of the file it sits in, so the
                            // site can be edited where it is read — but a member
                            // is routed by its handle when it has one, and a
                            // file handle spans the WHOLE file, so taking it
                            // here would turn a sweep into a whole-file rewrite.
                            node_id: (origin != "find_usages")
                                .then(|| r.node_id.clone())
                                .flatten(),
                            path: rel.to_string_lossy().into_owned(),
                            line: r.line.filter(|l| *l >= 1),
                        })
                    })
                    .collect()
            } else {
                Vec::new()
            };
        self.arm_found_set(sid, origin, members, total, results.len())
    }

    /// Store the set, or clear it when there is nothing addressable to store.
    ///
    /// The one place `Session::found_set` is written, so the on-disk copy cannot
    /// drift from the in-memory one: a session outlives the server process, and
    /// a FOUND set that survives only in RAM is one the next process silently
    /// loses.
    fn arm_found_set(
        &mut self,
        sid: &str,
        origin: &str,
        members: Vec<FoundMember>,
        total: usize,
        returned: usize,
    ) -> Option<String> {
        let root = self.sessions.get(sid)?.worktree_path.clone();
        if members.is_empty() {
            if let Some(session) = self.sessions.get_mut(sid) {
                session.found_set = None;
            }
            found_set::clear(&root);
            return None;
        }

        // A truncated result gets no master rev: without one every FOUND verb
        // refuses, which is the whole point — the rows beyond the cap were
        // never shown, and a set the agent did not see is not a set it chose.
        let complete = total == returned;
        let master_rev = if complete {
            self.master_rev_of(sid, &members).ok()
        } else {
            None
        };

        let set = FoundSet {
            origin: origin.to_string(),
            complete,
            master_rev: master_rev.clone(),
            members,
        };
        if let Err(err) = found_set::save(&set, &root) {
            tracing::warn!(
                error = %err,
                "could not persist the FOUND set; a server restart will lose it"
            );
        }
        if let Some(session) = self.sessions.get_mut(sid) {
            session.found_set = Some(set);
        }
        master_rev
    }

    /// `FIND files` — executed by the SHOW family (it renders a file listing),
    /// but it is a FIND, so it arms FOUND like the other two.
    ///
    /// Only handle-carrying rows arm it. A `GROUP BY` aggregate row is a count,
    /// not a node: there is nothing for a bulk verb to address and nothing for
    /// the master rev to fingerprint, so such a result clears FOUND instead of
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
                    .map(|f| FoundMember {
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
        let found_rev = self.arm_found_set(sid, "find_files", members, total, returned);

        // FIND files renders through the SHOW family, whose result has no
        // `found_rev` column of its own — the master rev rides in the metadata
        // map so the CSV row reads the same as it does on FIND symbols/usages.
        if let (Some(rev), ForgeQLResult::Show(show)) = (found_rev, &mut result) {
            drop(
                show.metadata
                    .get_or_insert_with(serde_json::Map::new)
                    .insert("found_rev".to_string(), serde_json::Value::String(rev)),
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
            None => Err(ForgeError::Rejection {
                kind: RejectionKind::NodeNotFound,
                payload: format!(
                    r#"{{"error":"node_not_found","node_id":"{node_id}","suggested_next":"SHOW outline OF file"}}"#
                ),
            }
            .into()),
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
            && (clauses.limit.is_some() || clauses.offset.is_some())
        {
            // OFFSET is honoured on its own, not only beside a LIMIT. Gating
            // the pair on `limit.is_some()` meant `SHOW body OF 'f' OFFSET 40`
            // returned lines 1-40 — precisely the page it asked to skip — and
            // every other verb applies OFFSET unconditionally.
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
