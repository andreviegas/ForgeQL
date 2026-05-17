use anyhow::Result;

use crate::{
    ir::{Backend, Clauses, GroupBy},
    result::{ForgeQLResult, QueryResult, ShowContent, ShowResult},
};

use super::ForgeQLEngine;
use super::{
    DEFAULT_QUERY_LIMIT, DEFAULT_SHOW_LINE_LIMIT, detect_metric_hint, reject_text_filter,
    require_session_id,
};
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
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        let metric_hint = detect_metric_hint(clauses);
        let group_by_field = match &clauses.group_by {
            Some(GroupBy::Field(f)) if f != "fql_kind" && f != "file" => Some(f.clone()),
            _ => None,
        };

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_symbols".to_string(),
            results,
            total,
            metric_hint,
            group_by_field,
        }))
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
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        Ok(ForgeQLResult::Query(QueryResult {
            op: "find_usages".to_string(),
            results,
            total,
            metric_hint: None,
            group_by_field: None,
        }))
    }

    // -------------------------------------------------------------------
    // Show-line cap helper
    // -------------------------------------------------------------------

    /// Apply all source-line output caps in one place.
    ///
    /// Must be called **after** WHERE predicates have been applied so that
    /// the counts reflect post-filter line totals.
    ///
    /// Caps applied in order:
    /// 1. Explicit `LIMIT` + `OFFSET` from the agent's clauses.
    /// 2. Implicit `DEFAULT_SHOW_LINE_LIMIT` block when no explicit `LIMIT`
    ///    was given — returns zero lines and a guidance hint.
    /// 3. Budget critical cap — truncates to `critical_max_lines` when the
    ///    session budget is in critical state.
    pub(super) fn apply_show_lines_cap(
        show_result: &mut ShowResult,
        clauses: Option<&Clauses>,
        budget_max: Option<usize>,
        is_explicit_range: bool,
    ) {
        // Operates only on source-line outputs.
        let total = match &show_result.content {
            ShowContent::Lines { lines, .. } => lines.len(),
            _ => return,
        };

        // ---- Explicit LIMIT + OFFSET, or implicit line cap ----
        if let Some(clauses) = clauses {
            if clauses.limit.is_some() {
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
            } else if !is_explicit_range && total > DEFAULT_SHOW_LINE_LIMIT {
                // No explicit LIMIT and output exceeds the cap.
                // Block the output entirely — return zero lines + guidance.
                if let ShowContent::Lines { lines, .. } = &mut show_result.content {
                    lines.clear();
                }
                show_result.total_lines = Some(total);
                show_result.hint = Some(format!(
                    "Blocked: this SHOW command would return {total} lines \
                     (limit is {DEFAULT_SHOW_LINE_LIMIT} without an explicit LIMIT clause). \
                     Use FIND symbols WHERE to locate the exact symbol you need — \
                     it returns file path and line numbers. \
                     Then use SHOW LINES n-m OF 'file' to read only those lines. \
                     If you really need all {total} lines, re-run with LIMIT {total}."
                ));
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
