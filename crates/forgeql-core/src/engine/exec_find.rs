#![allow(unused_imports)]
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use tracing::{debug, info, warn};

use crate::{
    ast::{index::SymbolTable, lang::LanguageRegistry, query, show},
    config::ForgeConfig,
    context::RequestContext,
    git::{
        self as git,
        source::{Source, SourceRegistry},
        worktree,
    },
    ir::{Clauses, ForgeQLIR},
    result::{
        BeginTransactionResult, CallDirection, CallGraphEntry, CommitResult, FileEntry,
        ForgeQLResult, MemberEntry, MutationResult, OutlineEntry, QueryResult, RollbackResult,
        ShowContent, ShowResult, SourceLine, SourceOpResult, SuggestionEntry, SymbolMatch,
        VerifyBuildResult,
    },
    session::{Checkpoint, Session, read_last_active},
    transforms::copy_move::{plan_copy_lines, plan_copy_lines_at, plan_move_lines},
    transforms::diff::{CompactDiffConfig, compact_diff_plan},
    transforms::{TransformPlan, plan_from_ir},
    verify,
    workspace::Workspace,
};

use super::ForgeQLEngine;
use super::{
    DEFAULT_QUERY_LIMIT, DEFAULT_SHOW_LINE_LIMIT, detect_metric_hint, find_symbols_prefilter,
    reject_text_filter, require_session_id, validate_order_by_field,
};

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
        let (mut results, remaining) = find_symbols_prefilter(index, clauses, root, &configs);

        validate_order_by_field(&remaining, &results, &configs)?;
        crate::filter::apply_clauses(&mut results, &remaining);

        let total = results.len();
        if clauses.limit.is_none() {
            results.truncate(DEFAULT_QUERY_LIMIT);
        }

        let metric_hint = detect_metric_hint(clauses);
        let group_by_field = match &clauses.group_by {
            Some(crate::ir::GroupBy::Field(f)) if f != "fql_kind" && f != "file" => Some(f.clone()),
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
