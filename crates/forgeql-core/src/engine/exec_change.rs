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
use super::{convert_suggestions, mutation_op_name, require_session_id};

impl ForgeQLEngine {
    pub(super) fn exec_mutation(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        let mut plan = {
            let (workspace, index) = self.require_workspace_and_index(session_id)?;
            plan_from_ir(op, &RequestContext::admin(), &workspace, index)?
        };

        let op_name = mutation_op_name(op);
        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();
        let lines_written = plan.lines_written();
        let suggestions = convert_suggestions(&plan);

        // Merge before generating preview (compact_diff_plan reads files).
        plan.merge_by_file()?;

        // Generate a compact diff preview *before* applying (apply consumes
        // the plan). Bounded by CompactDiffConfig defaults — at most K
        // content lines per file, each ≤ W characters wide.
        let diff = match compact_diff_plan(&plan, &CompactDiffConfig::default()) {
            Ok(d) if d.is_empty() => None,
            Ok(d) => Some(d),
            Err(_) => None,
        };

        let _ = plan.apply()?;

        // Reindex touched files.
        self.reindex_session(sid, &files_changed);

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            lines_written,
            diff,
            suggestions,
        }))
    }

    // ===================================================================
    // COPY / MOVE lines
    // ===================================================================

    pub(super) fn exec_copy_lines(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let (workspace, _index) = self.require_workspace_and_index(session_id)?;

        let (src, start, end, dst, at) = match op {
            ForgeQLIR::CopyLines {
                src,
                start,
                end,
                dst,
                at,
            } => (src.as_str(), *start, *end, dst.as_str(), *at),
            _ => bail!("exec_copy_lines called with wrong IR variant"),
        };

        let src_abs = workspace.safe_path(src)?;
        let dst_abs = workspace.safe_path(dst)?;

        let plan = match at {
            None => plan_copy_lines(src, &src_abs, start, end, &dst_abs)?,
            Some(at_line) => plan_copy_lines_at(src, &src_abs, start, end, &dst_abs, at_line)?,
        };

        self.apply_plan(sid, plan, "copy_lines")
    }

    pub(super) fn exec_move_lines(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let (workspace, _index) = self.require_workspace_and_index(session_id)?;

        let (src, start, end, dst, at) = match op {
            ForgeQLIR::MoveLines {
                src,
                start,
                end,
                dst,
                at,
            } => (src.as_str(), *start, *end, dst.as_str(), *at),
            _ => bail!("exec_move_lines called with wrong IR variant"),
        };

        let src_abs = workspace.safe_path(src)?;
        let dst_abs = workspace.safe_path(dst)?;

        let plan = plan_move_lines(src, &src_abs, start, end, &dst_abs, at)?;
        self.apply_plan(sid, plan, "move_lines")
    }

    /// Shared plan → diff → apply → reindex helper used by COPY and MOVE.
    fn apply_plan(
        &mut self,
        sid: &str,
        mut plan: TransformPlan,
        op_name: &str,
    ) -> Result<ForgeQLResult> {
        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();
        let lines_written = plan.lines_written();

        plan.merge_by_file()?;

        let diff = match compact_diff_plan(&plan, &CompactDiffConfig::default()) {
            Ok(d) if d.is_empty() => None,
            Ok(d) => Some(d),
            Err(_) => None,
        };

        let _ = plan.apply()?;

        self.reindex_session(sid, &files_changed);

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            lines_written,
            diff,
            suggestions: Vec::new(),
        }))
    }
    // Checkpoint-based transactions
    // ===================================================================
}
