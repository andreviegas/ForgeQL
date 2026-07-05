use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::{
    ir::{ChangeTarget, ForgeQLIR},
    result::{ForgeQLResult, MutationResult},
    transforms::change::lines_to_byte_range,
    transforms::copy_move::{plan_copy_lines, plan_copy_lines_at, plan_move_lines},
    transforms::diff::{CompactDiffConfig, compact_diff_addressed},
    transforms::{ByteRangeEdit, FileEdit, TransformPlan, plan_from_ir},
};

use super::ForgeQLEngine;
use super::{convert_suggestions, mutation_op_name, require_session_id};
use crate::ast::lang::LanguageRegistry;

impl ForgeQLEngine {
    pub(super) fn exec_mutation(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
        gate_indexed: bool,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;

        let mut plan = {
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            plan_from_ir(op, &workspace)?
        };

        // Experiment (temporary): only the user-facing CHANGE FILE / CHANGE FILES command
        // is blocked on indexed files (gate_indexed = true, set by the dispatch). Node
        // mutations (CHANGE NODE / DELETE NODE) route through here too but pass
        // gate_indexed = false, so they are never blocked. Override the block with
        // FORGEQL_ALLOW_CHANGE_FILE_INDEXED=1 (set by the VERIFY pre-commit script).
        // BUG-014: whole-file DELETION (`WITH NOTHING`) is exempt — it is not
        // raw-text editing of indexed content; the agent names the file
        // explicitly, the diff reports the deletion, and reindex prunes it.
        if gate_indexed
            && !matches!(
                op,
                ForgeQLIR::ChangeContent {
                    target: crate::ir::ChangeTarget::Delete,
                    ..
                }
            )
            && let Some(path) = first_blocked_indexed_path(
                plan.file_edits.iter().map(|fe| fe.path.as_path()),
                &self.lang_registry,
                change_file_indexed_allowed(),
            )
        {
            bail!(
                "CHANGE FILE is disabled for indexed files (temporary experiment): '{}' is an \
                 indexed source file. Edit it by node handle instead — locate it with FIND \
                 symbols or SHOW outline, then CHANGE NODE / INSERT NODE / DELETE NODE (append \
                 '(n-m)' to a node_id to splice a line range). Raw-text CHANGE FILE stays \
                 available for non-indexed files. Set FORGEQL_ALLOW_CHANGE_FILE_INDEXED=1 to \
                 re-enable.",
                path.display()
            );
        }

        let op_name = mutation_op_name(op);
        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();
        let lines_written = plan.lines_written();
        let suggestions = convert_suggestions(&plan);

        plan.merge_by_file()?;

        // Snapshot the merged edits before apply() consumes the plan, then apply.
        // apply() returns the pre-edit bytes of every modified file.
        let edits_snapshot = plan.file_edits.clone();
        let applied = plan.apply()?;

        // Reindex touched files.
        self.reindex_session(sid, &files_changed);

        // A successful mutation invalidates every commit gate: the agent must
        // re-run the gated VERIFY build(s) before COMMIT will accept the change.
        if let Some(session) = self.sessions.get_mut(sid) {
            session.satisfied_gates.clear();
            session.edits_since_gate = session.edits_since_gate.saturating_add(1);
        }

        // Build the diff AFTER apply + reindex so each present line carries an
        // inline `node_id(offset)` handle — the agent's BUG-022 self-correction
        // address — instead of an unaddressed pre-apply preview.
        let diff = self.build_post_edit_diff(sid, &edits_snapshot, &applied.originals);
        let lines_removed = crate::transforms::lines_removed(&edits_snapshot, &applied.originals);

        // Persist the pre-edit bytes to the per-session UNDO ring so this
        // mutation can be reversed with `UNDO`. Best-effort: a failed write only
        // means no undo is available, never a failed mutation.
        if let Ok((ws, _eng)) = self.require_workspace_and_engine(Some(sid)) {
            let _ = crate::undo::write_snapshot(ws.root(), op_name, &applied.originals);
        }

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            lines_written,
            lines_removed,
            diff,
            suggestions,
            new_node_id: None,
        }))
    }

    // ===================================================================
    // COPY / MOVE lines
    // ===================================================================

    /// Reject a `TO <dest>` argument that is purely numeric (BUG-016 footgun):
    /// `MOVE LINES 25-30 OF 'f.rs' TO 3` would create a file literally named
    /// `3` — a bare number almost certainly meant a line position, which is
    /// spelled `AT <line>`. Mechanical input validation, not path policy.
    fn reject_numeric_dest(dst: &str, verb: &str) -> Result<()> {
        if !dst.is_empty() && dst.chars().all(|c| c.is_ascii_digit()) {
            bail!(
                "{verb} TO '{dst}': destination must be a file path, not a number. \
             To position the lines within a file use {verb} … TO '<path>' AT {dst}."
            );
        }
        Ok(())
    }
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
        Self::reject_numeric_dest(dst, "COPY LINES")?;
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
        Self::reject_numeric_dest(dst, "MOVE LINES")?;
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

        // Snapshot the merged edits before apply() consumes the plan.
        let edits_snapshot = plan.file_edits.clone();
        let applied = plan.apply()?;
        self.reindex_session(sid, &files_changed);

        // Diff built after apply + reindex so it carries inline node addresses.
        let diff = self.build_post_edit_diff(sid, &edits_snapshot, &applied.originals);
        let lines_removed = crate::transforms::lines_removed(&edits_snapshot, &applied.originals);

        // Persist the pre-edit bytes to the per-session UNDO ring (see exec_mutation).
        if let Ok((ws, _eng)) = self.require_workspace_and_engine(Some(sid)) {
            let _ = crate::undo::write_snapshot(ws.root(), op_name, &applied.originals);
        }

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: op_name.to_string(),
            applied: true,
            files_changed,
            edit_count,
            lines_written,
            lines_removed,
            diff,
            suggestions: Vec::new(),
            new_node_id: None,
        }))
    }

    // ===================================================================
    // UNDO
    // ===================================================================

    /// Restore the files a recent mutation changed to their pre-edit bytes,
    /// reading the per-session UNDO ring (`last` = slot, 0 = most recent).
    ///
    /// Mechanical and language-agnostic: it rewrites the exact bytes `apply()`
    /// captured before the edit, reindexes the restored files, and invalidates
    /// the commit gate like any other mutation. `UNDO LAST-n` restores the slot
    /// `n` back, reversing the `n + 1` most recent mutations at once.
    pub(super) fn exec_undo(
        &mut self,
        session_id: Option<&str>,
        last: usize,
    ) -> Result<ForgeQLResult> {
        let sid = require_session_id(session_id)?;
        let root = {
            let (ws, _eng) = self.require_workspace_and_engine(session_id)?;
            ws.root().to_path_buf()
        };

        let Some(snapshot) = crate::undo::read_snapshot(&root, last)? else {
            bail!(
                "nothing to undo at LAST-{last}: the undo ring has no snapshot there \
                 (a mutation writes LAST-0; older mutations shift to LAST-1, LAST-2, …)"
            );
        };

        // Restore each captured file to its pre-edit bytes.
        let mut files_changed: Vec<PathBuf> = Vec::with_capacity(snapshot.files.len());
        for file in &snapshot.files {
            let abs = root.join(&file.rel_path);
            crate::workspace::file_io::write_atomic(&abs, &file.bytes)?;
            files_changed.push(abs);
        }

        // The working tree changed: reindex the restored files and invalidate
        // every commit gate, exactly as a mutation does.
        self.reindex_session(sid, &files_changed);
        if let Some(session) = self.sessions.get_mut(sid) {
            session.satisfied_gates.clear();
            session.edits_since_gate = session.edits_since_gate.saturating_add(1);
        }

        let mut summary = format!(
            "UNDO: restored {} file(s) to the state before '{}'",
            files_changed.len(),
            snapshot.op
        );
        for path in &files_changed {
            summary.push_str("\n  ");
            summary.push_str(&path.display().to_string());
        }

        Ok(ForgeQLResult::Mutation(MutationResult {
            op: "undo".to_string(),
            applied: true,
            edit_count: snapshot.files.len(),
            files_changed,
            lines_written: 0,
            lines_removed: 0,
            diff: Some(summary),
            suggestions: Vec::new(),
            new_node_id: None,
        }))
    }

    /// Build the post-edit mutation diff with inline `node_id(offset)` addresses
    /// on present lines.
    ///
    /// Must be called AFTER `plan.apply()` + `reindex_session` so the post-edit
    /// node ordinals exist. `originals` are the pre-edit bytes returned by
    /// `apply()`; `edits` is the pre-apply snapshot of the file edits. Returns
    /// `None` for an empty diff; gracefully degrades to an unaddressed diff when
    /// the session index is unavailable (the addresser then yields no handles).
    fn build_post_edit_diff(
        &self,
        sid: &str,
        edits: &[FileEdit],
        originals: &std::collections::HashMap<PathBuf, Vec<u8>>,
    ) -> Option<String> {
        let we = self.require_workspace_and_engine(Some(sid)).ok();
        let mut node_refs = |path: &Path, lo: usize, hi: usize| -> Vec<Option<(String, usize)>> {
            match &we {
                Some((ws, eng)) => {
                    let rel = ws.relative(path);
                    let rel_str = rel.to_string_lossy();
                    eng.innermost_nodes_for_lines(&rel_str, ws.root(), lo, hi)
                }
                None => Vec::new(),
            }
        };
        compact_diff_addressed(
            edits,
            originals,
            &CompactDiffConfig::default(),
            &mut node_refs,
        )
        .ok()
        .filter(|d| !d.is_empty())
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
        let NodeSpan {
            rel_path,
            node_line,
            start,
            end,
            ..
        } = self.resolve_node_span(session_id, node_id, if_rev)?;
        let ir = ForgeQLIR::ChangeContent {
            files: vec![rel_path.clone()],
            target: ChangeTarget::Lines {
                start,
                end,
                content: content.to_string(),
            },
            clauses: crate::ir::Clauses::default(),
        };
        let mut result = self.exec_mutation(session_id, &ir, false)?;
        // Re-resolve by the base node's start line (not its prior id) so the
        // caller learns the current handle even when the edit changed the node's
        // content_hash and the remapper assigned it a new ordinal.
        let sid = require_session_id(session_id)?;
        let new_node_id = {
            let session = self.require_session(sid)?;
            session
                .engine_for(&crate::ir::Backend::Default)?
                .find_node_id_at_line(&rel_path, node_line)
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
                delete: false,
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
        let NodeSpan {
            rel_path,
            node_end_line,
            start,
            end,
            has_offset,
            ..
        } = self.resolve_node_span(session_id, node_id, if_rev)?;
        // A whole-node delete absorbs the node's trailing blank line(s) so it
        // leaves no stray separator; an `id(n-m)` offset delete removes exactly
        // the addressed line range.
        let end = if has_offset {
            end
        } else {
            let session = self.require_session(require_session_id(session_id)?)?;
            std::fs::read_to_string(session.worktree_path.join(&rel_path))
                .map(|content| absorb_trailing_blank_lines(&content, node_end_line))
                .unwrap_or(node_end_line)
        };
        let ir = ForgeQLIR::ChangeContent {
            files: vec![rel_path],
            target: ChangeTarget::Lines {
                start,
                end,
                content: String::new(),
            },
            clauses: crate::ir::Clauses::default(),
        };
        self.exec_mutation(session_id, &ir, false)
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

    /// Resolve `id` or `id(n-m)` to the file + inclusive line span to operate on.
    /// Offset addressing lives here so CHANGE NODE and DELETE NODE stay in sync.
    /// The `IF REV` guard always covers the whole base node.
    fn resolve_node_span(
        &self,
        session_id: Option<&str>,
        node_id: &str,
        if_rev: Option<&str>,
    ) -> Result<NodeSpan> {
        let (base_id, offset) =
            crate::node_id::split_node_offset(node_id).map_err(|e| anyhow::anyhow!(e))?;
        let node = self.resolve_node(session_id, base_id, if_rev)?;
        let (start, end) = crate::node_id::offset_lines(node.line, node.end_line, offset)
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(NodeSpan {
            rel_path: node.rel_path,
            node_end_line: node.end_line,
            start,
            end,
            has_offset: offset.is_some(),
            node_line: node.line,
        })
    }
}

/// Scratch struct for resolved node location used by Phase C mutation helpers.
struct ResolvedNode {
    rel_path: String,
    line: usize,
    end_line: usize,
}

/// A node resolved to the line span an operation targets, honoring an optional
/// `(n-m)` offset suffix. Shared by CHANGE NODE and DELETE NODE so offset
/// addressing is defined in exactly one place.
struct NodeSpan {
    rel_path: String,
    /// Whole-node last line — used for trailing-blank absorption on a whole delete.
    node_end_line: usize,
    /// 1-based inclusive target span: the whole node, or the offset sub-range.
    start: usize,
    end: usize,
    /// True when an `(n-m)` suffix narrowed the span to a sub-range.
    has_offset: bool,
    /// 1-based start line of the base node, used to re-resolve the post-edit
    /// handle by position so the caller learns the new id even if it churned.
    node_line: usize,
}

/// Extract the inclusive 1-based line span `[line_start, line_end]` from `src`.
fn node_span_text(src: &str, line_start: usize, line_end: usize) -> String {
    src.lines()
        .skip(line_start.saturating_sub(1))
        .take(line_end.saturating_sub(line_start).saturating_add(1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extend a 1-based inclusive `end_line` forward over the contiguous run of
/// blank lines that immediately follow it in `content`, so deleting a node also
/// removes its trailing blank separator (avoids blank-line accumulation).
/// Whitespace is not part of a node's span/rev, so this only widens the DELETE
/// extent. Returns `end_line` unchanged when the next line is non-blank or out
/// of range.
fn absorb_trailing_blank_lines(content: &str, end_line: usize) -> usize {
    let lines: Vec<&str> = content.lines().collect();
    let mut end = end_line;
    // The 1-based line `end + 1` sits at 0-based index `end`.
    while end < lines.len() && lines[end].trim().is_empty() {
        end += 1;
    }
    end
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

/// True when `CHANGE FILE` on indexed files is permitted — escape hatch for the
/// test harness and anyone who opts back in.
fn change_file_indexed_allowed() -> bool {
    std::env::var_os("FORGEQL_ALLOW_CHANGE_FILE_INDEXED").is_some()
}

/// First indexed path among `paths` that the temporary `CHANGE FILE` block would
/// reject, or `None` when the edit is allowed (no indexed target, or `allow`).
fn first_blocked_indexed_path<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
    registry: &LanguageRegistry,
    allow: bool,
) -> Option<PathBuf> {
    if allow {
        return None;
    }
    paths
        .into_iter()
        .find(|p| registry.language_for_path(p).is_some())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod change_file_gate_tests {
    use super::first_blocked_indexed_path;
    use crate::ast::lang::{CppLanguageInline, LanguageRegistry};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn cpp_registry() -> LanguageRegistry {
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline)])
    }

    #[test]
    fn blocks_indexed_path_when_not_allowed() {
        let reg = cpp_registry();
        let p = PathBuf::from("src/foo.cpp");
        let blocked = first_blocked_indexed_path([p.as_path()], &reg, false);
        assert_eq!(blocked, Some(p));
    }

    #[test]
    fn allows_non_indexed_path() {
        let reg = cpp_registry();
        let p = PathBuf::from("notes.txt");
        assert_eq!(first_blocked_indexed_path([p.as_path()], &reg, false), None);
    }

    #[test]
    fn allow_override_lets_indexed_through() {
        let reg = cpp_registry();
        let p = PathBuf::from("src/foo.cpp");
        assert_eq!(first_blocked_indexed_path([p.as_path()], &reg, true), None);
    }
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
    fn absorb_trailing_blank_lines_extends_over_blank_run() {
        // No trailing blank → unchanged.
        assert_eq!(super::absorb_trailing_blank_lines("a\nb\nc", 1), 1);
        // One trailing blank after line 1 → absorbs it (end 1 → 2).
        assert_eq!(super::absorb_trailing_blank_lines("a\n\nc", 1), 2);
        // Multiple trailing blanks → absorbs the whole run.
        assert_eq!(super::absorb_trailing_blank_lines("a\n\n\n\nc", 1), 4);
        // Node is the last line → nothing to absorb.
        assert_eq!(super::absorb_trailing_blank_lines("a", 1), 1);
        // Trailing blank at EOF.
        assert_eq!(super::absorb_trailing_blank_lines("a\n\n", 1), 2);
        // Whitespace-only line counts as blank.
        assert_eq!(super::absorb_trailing_blank_lines("a\n   \nc", 1), 2);
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
