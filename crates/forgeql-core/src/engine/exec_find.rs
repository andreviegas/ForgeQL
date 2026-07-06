use anyhow::Result;

use crate::{
    ir::{Backend, Clauses, GroupBy},
    result::{ForgeQLResult, QueryResult, ShowContent, ShowResult},
};

use super::ForgeQLEngine;
use super::{detect_metric_hint, reject_text_filter, require_session_id};
impl ForgeQLEngine {
    pub(super) fn find_symbols(
        &self,
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

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results,
            total,
            metric_hint,
            group_by_field,
            hint,
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
        const CORE_WHERE_FIELDS: &[&str] = &[
            "name",
            "fql_kind",
            "kind",
            "node_kind",
            "path",
            "file",
            "line",
            "usages",
            "count",
            "language",
            "lang",
            "extension",
            "size",
            "depth",
            "signature",
            "value",
            "type",
            "body",
        ];
        if !results.is_empty() {
            return None;
        }
        for pred in &clauses.where_predicates {
            let field = pred.field.as_str();
            if CORE_WHERE_FIELDS.contains(&field) {
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
        &self,
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

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results,
            total,
            metric_hint: None,
            group_by_field: None,
            hint: None,
        }))
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
