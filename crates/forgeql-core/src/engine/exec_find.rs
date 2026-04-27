use std::collections::HashMap;

use anyhow::Result;

use crate::{
    ast::query,
    ir::{Clauses, GroupBy},
    result::{ForgeQLResult, QueryResult, ShowContent, ShowResult, SymbolMatch},
};

use super::ForgeQLEngine;
use super::{
    DEFAULT_QUERY_LIMIT, DEFAULT_SHOW_LINE_LIMIT, detect_metric_hint, find_symbols_prefilter,
    reject_text_filter, require_session_id, validate_order_by_field,
};

/// Try to answer a `FIND symbols GROUP BY <field>` query entirely from
/// pre-aggregated `IndexStats` without scanning individual rows.
///
/// Returns `None` when the query has WHERE predicates, IN/EXCLUDE globs, or
/// targets a field not covered by `IndexStats`.
fn try_group_by_stats_fast_path(
    index: &crate::ast::index::SymbolTable,
    clauses: &Clauses,
) -> Option<(Vec<SymbolMatch>, Clauses)> {
    // Must have a GROUP BY on a supported field, no WHERE filters, no globs.
    if !clauses.where_predicates.is_empty()
        || clauses.in_glob.is_some()
        || clauses.exclude_glob.is_some()
    {
        return None;
    }

    let group_field = match &clauses.group_by {
        Some(GroupBy::Field(f)) => f.clone(),
        _ => return None,
    };

    // IndexStats keys are interned u32 IDs — resolve to strings at output time.
    let map: Vec<(String, usize)> = match group_field.as_str() {
        "fql_kind" => index
            .stats
            .resolved_by_fql_kind(&index.strings)
            .into_iter()
            .collect(),
        "language" | "lang" => index
            .stats
            .resolved_by_language(&index.strings)
            .into_iter()
            .collect(),
        _ => return None,
    };

    let results: Vec<SymbolMatch> = map
        .into_iter()
        .map(|(key, count)| {
            let fql_kind = if group_field == "fql_kind" {
                Some(key.clone())
            } else {
                None
            };
            let language = if group_field == "language" || group_field == "lang" {
                Some(key.clone())
            } else {
                None
            };
            SymbolMatch {
                name: key,
                node_kind: None,
                fql_kind,
                language,
                path: None,
                line: None,
                usages_count: None,
                fields: HashMap::new(),
                count: Some(count),
            }
        })
        .collect();

    // Remaining clauses: HAVING, ORDER BY, OFFSET, LIMIT — group_by already consumed.
    let remaining = Clauses {
        where_predicates: Vec::new(),
        having_predicates: clauses.having_predicates.clone(),
        order_by: clauses.order_by.clone(),
        group_by: None,
        limit: clauses.limit,
        offset: clauses.offset,
        in_glob: None,
        exclude_glob: None,
        depth: None,
    };

    Some((results, remaining))
}
impl ForgeQLEngine {
    pub(super) fn find_symbols(
        &self,
        session_id: Option<&str>,
        clauses: &Clauses,
    ) -> Result<ForgeQLResult> {
        reject_text_filter(clauses)?;
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let index = session
            .index()
            .ok_or_else(|| anyhow::anyhow!("session index not ready — retry USE"))?;
        let root = &session.worktree_path;

        let configs = self.lang_registry.configs();

        // Fast path: GROUP BY fql_kind / language with no WHERE/IN/EXCLUDE —
        // answered from pre-aggregated IndexStats in O(groups) instead of O(rows).
        if let Some((mut results, remaining)) = try_group_by_stats_fast_path(index, clauses) {
            crate::filter::apply_clauses(&mut results, &remaining);
            let total = results.len();
            if clauses.limit.is_none() {
                results.truncate(DEFAULT_QUERY_LIMIT);
            }
            let metric_hint = detect_metric_hint(clauses);
            let group_by_field = match &clauses.group_by {
                Some(GroupBy::Field(f)) if f != "fql_kind" && f != "file" => Some(f.clone()),
                _ => None,
            };
            return Ok(ForgeQLResult::Query(QueryResult {
                op: "find_symbols".to_string(),
                results,
                total,
                metric_hint,
                group_by_field,
            }));
        }

        let (mut results, remaining) = find_symbols_prefilter(index, clauses, root, &configs);

        validate_order_by_field(&remaining, &results, &configs)?;
        crate::filter::apply_clauses(&mut results, &remaining);

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
        clauses: &Clauses,
    ) -> Result<ForgeQLResult> {
        reject_text_filter(clauses)?;
        let sid = require_session_id(session_id)?;
        let session = self.require_session(sid)?;
        let index = session
            .index()
            .ok_or_else(|| anyhow::anyhow!("session index not ready — retry USE"))?;
        let root = &session.worktree_path;

        let sites = query::find_usages(index, of);
        let mut results: Vec<SymbolMatch> = sites
            .iter()
            .filter(|site| {
                if let Some(ref glob) = clauses.in_glob
                    && !crate::ast::query::relative_glob_matches(&site.path, glob, root)
                {
                    return false;
                }
                if let Some(ref glob) = clauses.exclude_glob
                    && crate::ast::query::relative_glob_matches(&site.path, glob, root)
                {
                    return false;
                }
                true
            })
            .map(|site| SymbolMatch {
                name: of.to_string(),
                node_kind: None,
                fql_kind: None,
                language: None,
                path: Some(site.path.clone()),
                line: Some(site.line),
                usages_count: None,
                fields: std::collections::HashMap::new(),
                count: None,
            })
            .collect();

        // Strip IN/EXCLUDE from clauses — already applied above.
        let remaining = Clauses {
            in_glob: None,
            exclude_glob: None,
            ..clauses.clone()
        };

        let configs = self.lang_registry.configs();
        validate_order_by_field(&remaining, &results, &configs)?;

        crate::filter::apply_clauses(&mut results, &remaining);
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
