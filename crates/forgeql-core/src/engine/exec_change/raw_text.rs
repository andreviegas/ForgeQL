//! `CHANGE FILE` — raw-text mutations on non-indexed files, guarded by the
//! indexed-file gate — and the `COPY LINES` / `MOVE LINES` verbs.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::ast::lang::LanguageRegistry;
use crate::engine::{ForgeQLEngine, convert_suggestions, mutation_op_name, require_session_id};
use crate::ir::ForgeQLIR;
use crate::result::{ForgeQLResult, MutationResult};
use crate::transforms::copy_move::{plan_copy_lines, plan_copy_lines_at, plan_move_lines};
use crate::transforms::plan_from_ir;

impl ForgeQLEngine {
    pub(in crate::engine) fn exec_mutation(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
        gate_indexed: bool,
    ) -> Result<ForgeQLResult> {
        let result = self.exec_mutation_inner(session_id, op, gate_indexed);
        self.discard_tombstones_if_err(session_id, &result);
        result
    }
    pub(in crate::engine) fn exec_mutation_inner(
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
        let structural_before = self.structural_validity(sid, &files_changed);
        let applied = plan.apply()?;

        // Paths this plan brought into existence (files and their engine-made
        // ancestor directories) must be recorded, or ROLLBACK cannot remove
        // them — `git reset --hard` walks straight past untracked paths.
        self.record_created(sid, &applied.created);

        // Reindex touched files.
        self.reindex_session(sid, &files_changed);
        let structural_errors = self.structural_errors(sid, &files_changed, &structural_before);

        // A successful mutation invalidates every commit gate: the agent must
        // re-run the gated VERIFY build(s) before COMMIT will accept the change.
        self.invalidate_found_set(sid);
        if let Some(session) = self.sessions.get_mut(sid) {
            session.satisfied_gates.clear();
            session.edits_since_gate = session.edits_since_gate.saturating_add(1);
            session.mutation_seq = session.mutation_seq.saturating_add(1);
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
            new_rev: None,
            structural_errors,
        }))
    }
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
    pub(in crate::engine) fn exec_copy_lines(
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

        self.apply_plan(sid, plan, "copy_lines", Some((start, end)))
    }
    pub(in crate::engine) fn exec_move_lines(
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

        let plan = plan_move_lines(src, &src_abs, start, end, end, &dst_abs, at)?;
        self.apply_plan(sid, plan, "move_lines", Some((start, end)))
    }
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
