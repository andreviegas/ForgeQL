use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::{
    ir::{ChangeTarget, ForgeQLIR},
    result::{ForgeQLResult, MutationResult},
    transforms::change::lines_to_byte_range,
    transforms::copy_move::{plan_copy_lines, plan_copy_lines_at, plan_move_lines},
    transforms::diff::{CompactDiffConfig, compact_diff_plan},
    transforms::{ByteRangeEdit, FileEdit, TransformPlan, plan_from_ir},
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
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            plan_from_ir(op, &workspace)?
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
            new_node_id: None,
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
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;

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
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;

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
            new_node_id: None,
        }))
    }
    // =================================================================
    // Node-addressed mutations (Phase C)
    // =================================================================

    pub(super) fn exec_change_node(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (node_id, if_rev, content) = match op {
            ForgeQLIR::ChangeNode {
                node_id,
                if_rev,
                content,
            } => (node_id.as_str(), if_rev.as_deref(), content.as_str()),
            _ => bail!("exec_change_node called with wrong IR variant"),
        };
        // A node_id may carry a node-relative line offset suffix — `id(n)` or
        // `id(n-m)`. The `IF REV` guard always checks the whole node's rev, so
        // resolve the base node first, then narrow the spliced line range.
        let (base_id, offset) =
            crate::node_id::split_node_offset(node_id).map_err(|e| anyhow::anyhow!(e))?;
        let node = self.resolve_node(session_id, base_id, if_rev)?;
        let (start, end) = crate::node_id::offset_lines(node.line, node.end_line, offset)
            .map_err(|e| anyhow::anyhow!(e))?;
        let ir = ForgeQLIR::ChangeContent {
            files: vec![node.rel_path],
            target: ChangeTarget::Lines {
                start,
                end,
                content: content.to_string(),
            },
            clauses: crate::ir::Clauses::default(),
        };
        let mut result = self.exec_mutation(session_id, &ir)?;
        // After reindex the ordinal is stable — confirm with a lookup and
        // return the (unchanged) node_id so callers don't need a follow-up query.
        let sid = require_session_id(session_id)?;
        let new_node_id = {
            let session = self.require_session(sid)?;
            let root = session.worktree_path.clone();
            session
                .engine_for(&crate::ir::Backend::Default)?
                .find_node(base_id, &root)
                .ok()
                .flatten()
                .map(|n| n.node_id)
        };
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.new_node_id = new_node_id;
        }
        Ok(result)
    }

    pub(super) fn exec_insert_node(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (node_id, before, content) = match op {
            ForgeQLIR::InsertNode {
                node_id,
                before,
                content,
            } => (node_id.as_str(), *before, content.as_str()),
            _ => bail!("exec_insert_node called with wrong IR variant"),
        };

        let sid = require_session_id(session_id)?;
        let node = self.resolve_node(session_id, node_id, None)?;
        // Line where the inserted content will land after reindex.
        let insert_line = if before { node.line } else { node.end_line + 1 };

        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let abs_path = workspace.safe_path(&node.rel_path)?;
        let file_bytes = crate::workspace::file_io::read_bytes(&abs_path)?;

        // Byte offset: BEFORE = start of the node's first line;
        //              AFTER  = byte just past the node's last line (incl. '\n').
        let insert_offset = if before {
            lines_to_byte_range(&file_bytes, node.line, node.line)?.0
        } else {
            lines_to_byte_range(&file_bytes, node.end_line, node.end_line)?.1
        };

        let insertion = if content.ends_with('\n') {
            content.to_string()
        } else {
            format!("{content}\n")
        };

        let plan = TransformPlan {
            file_edits: vec![FileEdit {
                path: abs_path,
                edits: vec![ByteRangeEdit::new(insert_offset..insert_offset, insertion)],
            }],
            suggestions: Vec::new(),
        };
        let mut result = self.apply_plan(sid, plan, "insert_node")?;

        // After reindex, find the first symbol at the insertion line.
        let new_node_id = {
            let session = self.require_session(sid)?;
            session
                .engine_for(&crate::ir::Backend::Default)?
                .find_node_id_at_line(&node.rel_path, insert_line)
        };
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.new_node_id = new_node_id;
        }
        Ok(result)
    }

    pub(super) fn exec_delete_node(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (node_id, if_rev) = match op {
            ForgeQLIR::DeleteNode { node_id, if_rev } => (node_id.as_str(), if_rev.as_deref()),
            _ => bail!("exec_delete_node called with wrong IR variant"),
        };
        let node = self.resolve_node(session_id, node_id, if_rev)?;
        let ir = ForgeQLIR::ChangeContent {
            files: vec![node.rel_path],
            target: ChangeTarget::Lines {
                start: node.line,
                end: node.end_line,
                content: String::new(),
            },
            clauses: crate::ir::Clauses::default(),
        };
        self.exec_mutation(session_id, &ir)
    }

    /// Resolve `node_id` → (`rel_path`, `line`, `end_line`) and optionally check IF REV guard.
    fn resolve_node(
        &self,
        session_id: Option<&str>,
        node_id: &str,
        if_rev: Option<&str>,
    ) -> Result<ResolvedNode> {
        let session = self.require_session(require_session_id(session_id)?)?;
        let root = session.worktree_path.clone();
        let node = session
            .engine_for(&crate::ir::Backend::Default)?
            .find_node(node_id, &root)?
            .ok_or_else(|| {
                anyhow::anyhow!(r#"{{"error":"node_not_found","node_id":"{node_id}"}}"#)
            })?;
        if let Some(expected) = if_rev
            && node.rev != expected
        {
            // Self-healing rejection: hand back the node's current rev, line
            // range, and source so the agent can re-target without a follow-up
            // read. The guard always covers the whole node.
            let current_content = std::fs::read_to_string(&node.path)
                .ok()
                .map(|src| node_span_text(&src, node.line, node.end_line))
                .unwrap_or_default();
            let payload = rev_mismatch_payload(
                node_id,
                expected,
                &node.rev,
                node.line,
                node.end_line,
                &current_content,
            );
            bail!("{payload}");
        }
        let rel_path = node
            .path
            .strip_prefix(&root)
            .unwrap_or(&node.path)
            .to_string_lossy()
            .into_owned();
        Ok(ResolvedNode {
            rel_path,
            line: node.line,
            end_line: node.end_line,
        })
    }
}

/// Scratch struct for resolved node location used by Phase C mutation helpers.
struct ResolvedNode {
    rel_path: String,
    line: usize,
    end_line: usize,
}

/// Extract the inclusive 1-based line span `[line_start, line_end]` from `src`.
fn node_span_text(src: &str, line_start: usize, line_end: usize) -> String {
    src.lines()
        .skip(line_start.saturating_sub(1))
        .take(line_end.saturating_sub(line_start).saturating_add(1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the self-healing rejection payload for a failed `CHANGE NODE … IF REV`
/// guard. Carries the node's current rev, line range, and source so the agent
/// can re-target the edit without a follow-up read.
fn rev_mismatch_payload(
    node_id: &str,
    expected: &str,
    current_rev: &str,
    line_start: usize,
    line_end: usize,
    current_content: &str,
) -> serde_json::Value {
    serde_json::json!({
        "error": "rev_mismatch",
        "node_id": node_id,
        "expected": expected,
        "current_rev": current_rev,
        "line_start": line_start,
        "line_end": line_end,
        "current_content": current_content,
    })
}

#[cfg(test)]
mod rev_mismatch_tests {
    use super::{node_span_text, rev_mismatch_payload};

    #[test]
    fn node_span_text_extracts_inclusive_1based_range() {
        let src = "a\nb\nc\nd\ne";
        assert_eq!(node_span_text(src, 2, 4), "b\nc\nd");
        assert_eq!(node_span_text(src, 1, 1), "a");
        assert_eq!(node_span_text(src, 5, 5), "e");
        assert_eq!(node_span_text(src, 1, 5), src);
    }

    #[test]
    fn rev_mismatch_payload_carries_self_healing_fields() {
        let payload = rev_mismatch_payload(
            "nabc123def456.0000",
            "hdeadbeefdeadbeef",
            "h0123456789abcdef",
            10,
            14,
            "int add() { return 1; }",
        );
        assert_eq!(payload["error"], "rev_mismatch");
        assert_eq!(payload["node_id"], "nabc123def456.0000");
        assert_eq!(payload["expected"], "hdeadbeefdeadbeef");
        assert_eq!(payload["current_rev"], "h0123456789abcdef");
        assert_eq!(payload["line_start"], 10);
        assert_eq!(payload["line_end"], 14);
        assert_eq!(payload["current_content"], "int add() { return 1; }");
    }
}
