use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::{
    ir::{ChangeTarget, ForgeQLIR},
    result::{ForgeQLResult, MutationResult},
    session::found_set::{self, FoundMember, FoundSet},
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

        // Paths this plan brought into existence (files and their engine-made
        // ancestor directories) must be recorded, or ROLLBACK cannot remove
        // them — `git reset --hard` walks straight past untracked paths.
        self.record_created(sid, &applied.created);

        // Reindex touched files.
        self.reindex_session(sid, &files_changed);

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

        self.apply_plan(sid, plan, "copy_lines", Some((start, end)))
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
        self.apply_plan(sid, plan, "move_lines", Some((start, end)))
    }

    /// Record paths this mutation brought into existence — and write the
    /// checkpoint stack to disk immediately.
    ///
    /// The list only matters to a ROLLBACK, and the ROLLBACK may well happen in
    /// a **different process**: sessions outlive the server, an agent can
    /// reconnect hours later, and the checkpoint file is the only thing that
    /// crosses a restart. Keeping created paths in RAM until BEGIN or ROLLBACK
    /// next writes the file would mean a restart silently forgets them — and a
    /// ROLLBACK that leaves created files behind is precisely the bug this list
    /// exists to fix. So the save is not an optimization to defer; it is the
    /// point.
    ///
    /// Paths are stored **worktree-relative**, so they survive the worktree
    /// being reopened at a different absolute location.
    ///
    /// Outside a transaction there is nothing to roll back to, so nothing is
    /// recorded.
    fn record_created(&mut self, sid: &str, created: &[PathBuf]) {
        if created.is_empty() {
            return;
        }
        let Some(session) = self.sessions.get_mut(sid) else {
            return;
        };
        let root = session.worktree_path.clone();
        let Some(checkpoint) = session.checkpoints.last_mut() else {
            return;
        };
        for abs in created {
            checkpoint
                .created
                .push(abs.strip_prefix(&root).unwrap_or(abs).to_path_buf());
        }
        let session = &*session;
        if let Err(err) = crate::session::checkpoint_file::save(session, &root) {
            tracing::warn!(
                error = %err,
                "could not persist created paths; a ROLLBACK after a restart may leave them behind"
            );
        }
    }
    /// Shared plan → diff → apply → reindex helper for plan-based mutations.
    ///
    /// `line_range` is the inclusive source line range the agent addressed
    /// (COPY/MOVE LINES); `None` keeps the payload-based line count.  When
    /// set, the reported `lines_written` (and, for MOVE, `lines_removed`) is
    /// the range's length rather than a count of the payload's text lines:
    /// the line-addressing model treats the position after a final newline
    /// as an addressable (zero-byte) line, so a whole-file copy addressed as
    /// `1-<count>` would otherwise report one line fewer than requested and
    /// read like data loss.
    fn apply_plan(
        &mut self,
        sid: &str,
        mut plan: TransformPlan,
        op_name: &str,
        line_range: Option<(usize, usize)>,
    ) -> Result<ForgeQLResult> {
        // Merge FIRST: a same-file relocation (MOVE NODE / MOVE LINES) arrives as
        // two FileEdits on one path, and collecting before the merge reported the
        // file twice.
        plan.merge_by_file()?;

        let files_changed: Vec<PathBuf> =
            plan.file_edits.iter().map(|fe| fe.path.clone()).collect();
        let edit_count = plan.edit_count();
        let range_len = line_range.map(|(start, end)| end.saturating_sub(start).saturating_add(1));
        let lines_written = range_len.unwrap_or_else(|| plan.lines_written());

        // Snapshot the merged edits before apply() consumes the plan.
        let edits_snapshot = plan.file_edits.clone();
        let applied = plan.apply()?;
        self.reindex_session(sid, &files_changed);

        // A successful mutation invalidates every commit gate and the
        // remembered FIND sites (line numbers may have shifted). This is the
        // shared bookkeeping for every plan-based mutation (insert/delete
        // node, node-scoped matching, the rename sweep).
        self.invalidate_found_set(sid);
        if let Some(session) = self.sessions.get_mut(sid) {
            session.satisfied_gates.clear();
            session.edits_since_gate = session.edits_since_gate.saturating_add(1);
            session.mutation_seq = session.mutation_seq.saturating_add(1);
        }
        // Files this plan brought into existence are untracked until COMMIT
        // stages them, so ROLLBACK's `git reset --hard` would walk straight past
        // them. Record them in the topmost frame — and on disk, since the
        // ROLLBACK may come from a process that has restarted since. Anything
        // created below a nested BEGIN was staged by that BEGIN, so the reset
        // already covers it.
        self.record_created(sid, &applied.created);

        // Diff built after apply + reindex so it carries inline node addresses.
        let diff = self.build_post_edit_diff(sid, &edits_snapshot, &applied.originals);
        // MOVE deletes exactly the addressed source range; report the same
        // range length on both counters so written == removed for a clean
        // move.  Every other op keeps the payload-based count.
        let lines_removed = match range_len {
            Some(len) if op_name == "move_lines" => len,
            _ => crate::transforms::lines_removed(&edits_snapshot, &applied.originals),
        };

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
            new_rev: None,
        }))
    }

    /// Stamp the post-edit handle **and its rev** onto a mutation result.
    ///
    /// The two always travel together. With `IF REV` mandatory, handing back a
    /// handle without its new rev would make every chained edit on the same node
    /// pay for a `FIND NODE` round trip first.
    fn stamp_new_handle(&self, sid: &str, result: &mut ForgeQLResult, node_id: Option<String>) {
        let rev = node_id.as_deref().and_then(|id| {
            let session = self.require_session(sid).ok()?;
            let root = session.worktree_path.clone();
            session
                .engine_for(&crate::ir::Backend::Default)
                .ok()?
                .find_node(id, &root)
                .ok()?
                .map(|n| n.rev)
        });
        if let ForgeQLResult::Mutation(ref mut m) = *result {
            m.new_node_id = node_id;
            m.new_rev = rev;
        }
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
        self.invalidate_found_set(sid);
        if let Some(session) = self.sessions.get_mut(sid) {
            session.satisfied_gates.clear();
            session.edits_since_gate = session.edits_since_gate.saturating_add(1);
            session.mutation_seq = session.mutation_seq.saturating_add(1);
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
            new_rev: None,
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
        let if_rev = Some(require_rev(if_rev, "CHANGE NODE", node_id)?);
        let NodeSpan {
            rel_path,
            node_line,
            start,
            end,
            has_offset,
            kind,
            ..
        } = self.resolve_node_span(session_id, node_id, if_rev)?;
        if is_path_kind(&kind) && !has_offset {
            // Overwriting a whole file destroys everything in it; a directory
            // has no content to overwrite at all.
            if kind == "dir" {
                bail!(
                    "CHANGE NODE '{node_id}' addresses a directory — a directory \
                     has no content. Address a file or a node inside it."
                );
            }
            require_path_rev("CHANGE", node_id, &kind, if_rev)?;
        }
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
                // A whole-path handle is derived from the path, so an edit cannot
                // change it — and there is no symbol at the line to re-derive it
                // from. Fall back to the handle the agent addressed.
                .or_else(|| Some(node_id.to_string()))
        };
        self.stamp_new_handle(sid, &mut result, new_node_id);
        Ok(result)
    }

    /// `CHANGE NODE 'id' IF REV MATCHING [WORD] 'a' WITH 'b'` — replace
    /// pattern occurrences inside the node's current line span only.
    pub(super) fn exec_change_node_matching(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (node_id, if_rev, pattern, replacement, word_boundary) = match op {
            ForgeQLIR::ChangeNodeMatching {
                node_id,
                if_rev,
                pattern,
                replacement,
                word_boundary,
            } => (
                node_id.as_str(),
                if_rev.as_deref(),
                pattern.as_str(),
                replacement.as_str(),
                *word_boundary,
            ),
            _ => bail!("exec_change_node_matching called with wrong IR variant"),
        };
        let if_rev = Some(require_rev(if_rev, "CHANGE NODE … MATCHING", node_id)?);
        let NodeSpan {
            rel_path,
            node_line,
            start,
            end,
            ..
        } = self.resolve_node_span(session_id, node_id, if_rev)?;
        let sid = require_session_id(session_id)?;

        let plan = {
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            let abs_path = workspace.safe_path(&rel_path)?;
            let file_bytes = crate::workspace::file_io::read_bytes(&abs_path)?;
            let (span_start, span_end) = lines_to_byte_range(&file_bytes, start, end)?;
            let edits = crate::transforms::matching_edits_in_range(
                &file_bytes,
                pattern,
                replacement,
                word_boundary,
                span_start..span_end,
            )?;
            if edits.is_empty() {
                bail!(
                    "no occurrences of '{pattern}' inside node {node_id} \
                     ({rel_path} lines {start}-{end})"
                );
            }
            TransformPlan {
                file_edits: vec![FileEdit {
                    path: abs_path,
                    edits,
                    delete: false,
                }],
                suggestions: Vec::new(),
            }
        };

        let mut result = self.apply_plan(sid, plan, "change_node_matching", None)?;
        let new_node_id = {
            let session = self.require_session(sid)?;
            session
                .engine_for(&crate::ir::Backend::Default)?
                .find_node_id_at_line(&rel_path, node_line)
                // A whole-path handle is derived from the path, so an edit cannot
                // change it — and there is no symbol at the line to re-derive it
                // from. Fall back to the handle the agent addressed.
                .or_else(|| Some(node_id.to_string()))
        };
        self.stamp_new_handle(sid, &mut result, new_node_id);
        Ok(result)
    }

    /// Drop the armed set, in RAM and on disk, in one place.
    ///
    /// A mutation shifts line numbers, so the set no longer points at what the
    /// agent saw. Clearing only the in-memory copy would leave the file behind
    /// for the next process to restore — a stale set that looks live.
    fn invalidate_found_set(&mut self, sid: &str) {
        let Some(session) = self.sessions.get_mut(sid) else {
            return;
        };
        // Clear the file unconditionally, not just when this process happened to
        // hold the set in RAM. A process that never restored it would otherwise
        // leave the file behind, and the next reconnect would resurrect a set
        // whose members this very mutation has already moved — stale handles,
        // offered to verbs that do not all demand a rev.
        drop(session.found_set.take());
        found_set::clear(&session.worktree_path);
    }

    /// The armed set, before any rev check: the two refusals that do not depend
    /// on what the code looks like now.
    ///
    /// Split from the rev check so a verb can reject a set it could never act on
    /// (usage sites are not nodes) before demanding a matching rev — the agent
    /// should not have to fix its rev to be told the set was the wrong shape.
    fn armed_found_set(&self, session_id: Option<&str>) -> Result<FoundSet> {
        let sid = require_session_id(session_id)?;
        let set = self
            .require_session(sid)?
            .found_set
            .clone()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no FIND result is armed in this session — run FIND symbols/usages/files \
                     first, then re-issue the FOUND command"
                )
            })?;

        if !set.complete {
            bail!(
                "the previous {} was truncated, so no master rev was issued for it — a FOUND \
                 mutation would act on rows you were never shown. Re-run the FIND with a LIMIT \
                 that covers the whole result (or narrower filters), then repeat this command.",
                set.origin
            );
        }
        Ok(set)
    }

    /// Re-derive the master rev from the live members and compare.
    fn verify_found_rev(
        &self,
        session_id: Option<&str>,
        set: &FoundSet,
        if_rev: Option<&str>,
    ) -> Result<()> {
        let Some(expected) = if_rev else {
            return Ok(());
        };
        let sid = require_session_id(session_id)?;
        let current = self.master_rev_of(sid, &set.members)?;
        if expected != current {
            // No current rev in the payload, unlike the single-node gate: there,
            // the agent can see what it is about to overwrite in the returned
            // span. A set is N nodes it cannot see, so the only safe recovery is
            // to look again — re-running the FIND both re-shows the rows and
            // issues the rev that matches them.
            bail!(
                r#"{{"error":"rev_mismatch","scope":"last","expected":"{expected}","members":{},"origin":"{}","suggested_next":"the set moved since the FIND armed it — re-run the FIND to see the current rows and get a fresh master rev"}}"#,
                set.members.len(),
                set.origin
            );
        }
        Ok(())
    }

    /// Live `(key, rev)` for every member of a set.
    ///
    /// Read fresh on both sides of the gate — FIND to issue the master rev, the
    /// mutation to re-derive it — because a rev cached at FIND time proves only
    /// that FIND ran, not that the code still looks the way it did.
    ///
    /// A member that has since been deleted reads as `gone`, which flips the
    /// hash exactly as an edit would: it is a change to the set either way.
    fn found_set_revs(&self, sid: &str, members: &[FoundMember]) -> Result<Vec<(String, String)>> {
        let session = self.require_session(sid)?;
        let root = session.worktree_path.clone();
        let engine = session.engine_for(&crate::ir::Backend::Default)?;
        members
            .iter()
            .map(|m| {
                let rev = match &m.node_id {
                    Some(id) => engine
                        .find_node(id, &root)?
                        .map_or_else(|| "gone".to_string(), |n| n.rev),
                    // A usage site has no handle of its own — the file it sits
                    // in stands in for it, which also catches an edit that
                    // moved the line out from under it.
                    None => std::fs::read(root.join(&m.path)).map_or_else(
                        |_| "gone".to_string(),
                        |bytes| crate::node_id::format_rev(crate::node_id::rev_of_bytes(&bytes)),
                    ),
                };
                Ok((m.key(), rev))
            })
            .collect()
    }

    /// The master rev of a member list as it stands right now.
    ///
    /// The single place a master rev is derived, so the rev FIND issues and the
    /// rev the gate compares against can never be computed two different ways.
    pub(super) fn master_rev_of(&self, sid: &str, members: &[FoundMember]) -> Result<String> {
        Ok(FoundSet::master_rev(&self.found_set_revs(sid, members)?))
    }

    /// The 1-based inclusive line span each member contributes to a sweep.
    ///
    /// A handle contributes its whole node span (for a file handle, the whole
    /// file); a usage site contributes its single line. Spans are merged per
    /// file so two overlapping members cannot produce two edits over the same
    /// bytes.
    fn found_set_spans(
        &self,
        session_id: Option<&str>,
        set: &FoundSet,
    ) -> Result<std::collections::BTreeMap<String, Vec<(usize, usize)>>> {
        let mut by_file: std::collections::BTreeMap<String, Vec<(usize, usize)>> =
            std::collections::BTreeMap::new();
        for member in &set.members {
            let (path, span) = if let Some(id) = &member.node_id {
                let span = self.resolve_node_span(session_id, id, None)?;
                (span.rel_path, (span.start, span.end))
            } else {
                let line = member.line.ok_or_else(|| {
                    anyhow::anyhow!(
                        "a FOUND member has neither a node handle nor a line: {}",
                        member.path
                    )
                })?;
                (member.path.clone(), (line, line))
            };
            by_file.entry(path).or_default().push(span);
        }
        for spans in by_file.values_mut() {
            spans.sort_unstable();
            let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
            for (start, end) in spans.iter().copied() {
                match merged.last_mut() {
                    Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                    _ => merged.push((start, end)),
                }
            }
            *spans = merged;
        }
        Ok(by_file)
    }

    /// Every member's path, in FIND order, for the verbs that act on whole
    /// files. A usage site addresses a line, not a file: it is not a thing that
    /// can be deleted or moved, and saying so beats silently deleting the file
    /// that happened to contain it.
    fn found_set_paths(set: &FoundSet, verb: &str) -> Result<Vec<String>> {
        if let Some(site) = set.members.iter().find(|m| m.node_id.is_none()) {
            bail!(
                "{verb} NODES FOUND needs addressable nodes, but the set came from {} — its rows \
                 are call sites (a line in {}), not nodes. Re-run as FIND files (or FIND symbols) \
                 to arm a set of handles.",
                set.origin,
                site.path
            );
        }
        Ok(set.members.iter().map(|m| m.path.clone()).collect())
    }

    pub(super) fn exec_change_nodes_found(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (pattern, replacement, word_boundary, if_rev) = match op {
            ForgeQLIR::ChangeNodesFound {
                pattern,
                replacement,
                word_boundary,
                if_rev,
            } => (
                pattern.as_str(),
                replacement.as_str(),
                *word_boundary,
                if_rev.as_deref(),
            ),
            _ => bail!("exec_change_nodes_found called with wrong IR variant"),
        };
        let sid = require_session_id(session_id)?;
        // Set first, then the gate: "you have nothing armed" is a more useful
        // thing to be told than "you forgot a rev" when there is no set at all.
        let set = self.armed_found_set(session_id)?;
        let if_rev = require_found_rev(if_rev, "CHANGE")?;
        self.verify_found_rev(session_id, &set, Some(if_rev))?;
        let member_count = set.members.len();
        let spans = self.found_set_spans(session_id, &set)?;

        let plan = {
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            let mut file_edits = Vec::new();
            for (rel_path, ranges) in spans {
                let abs_path = workspace.safe_path(&rel_path)?;
                let file_bytes = crate::workspace::file_io::read_bytes(&abs_path)?;
                let mut edits = Vec::new();
                for (start, end) in ranges {
                    let (span_start, span_end) = lines_to_byte_range(&file_bytes, start, end)?;
                    edits.extend(crate::transforms::matching_edits_in_range(
                        &file_bytes,
                        pattern,
                        replacement,
                        word_boundary,
                        span_start..span_end,
                    )?);
                }
                if edits.is_empty() {
                    continue;
                }
                file_edits.push(FileEdit {
                    path: abs_path,
                    edits,
                    delete: false,
                });
            }
            if file_edits.is_empty() {
                bail!(
                    "no occurrences of '{pattern}' within the {member_count} node(s) of the \
                     previous {} result",
                    set.origin
                );
            }
            TransformPlan {
                file_edits,
                suggestions: Vec::new(),
            }
        };

        self.apply_plan(sid, plan, "change_nodes_found", None)
    }

    /// `DELETE NODES FOUND IF REV 'master'` — unlink every member of the set.
    ///
    /// Lowered to the same whole-path delete as `DELETE NODE`, but as one plan:
    /// a half-applied bulk delete is not something an agent can reason about.
    pub(super) fn exec_delete_nodes_found(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let if_rev = match op {
            ForgeQLIR::DeleteNodesFound { if_rev } => if_rev.as_deref(),
            _ => bail!("exec_delete_nodes_found called with wrong IR variant"),
        };
        let if_rev = require_found_rev(if_rev, "DELETE")?;
        let set = self.armed_found_set(session_id)?;
        let paths = Self::found_set_paths(&set, "DELETE")?;
        self.verify_found_rev(session_id, &set, Some(if_rev))?;

        // A directory member expands to the files under it, exactly as the
        // single-node recursive delete does.
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let root = workspace.root().to_path_buf();
        let mut files: Vec<String> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        for rel in &paths {
            let abs = workspace.safe_path(rel)?;
            if abs.is_dir() {
                dirs.push(abs.clone());
                files.extend(
                    workspace
                        .files()
                        .filter(|p| !crate::result::FileEntry::is_runtime_artifact(p))
                        .filter(|p| p.starts_with(&abs))
                        .map(|p| {
                            p.strip_prefix(&root)
                                .unwrap_or(&p)
                                .to_string_lossy()
                                .into_owned()
                        }),
                );
            } else {
                files.push(rel.clone());
            }
        }
        files.sort_unstable();
        files.dedup();

        let mut result = if files.is_empty() {
            ForgeQLResult::Mutation(MutationResult {
                op: "delete_nodes_found".to_string(),
                applied: true,
                files_changed: Vec::new(),
                edit_count: 0,
                lines_written: 0,
                lines_removed: 0,
                diff: None,
                suggestions: Vec::new(),
                new_node_id: None,
                new_rev: None,
            })
        } else {
            let ir = ForgeQLIR::ChangeContent {
                files,
                target: ChangeTarget::Delete,
                clauses: crate::ir::Clauses::default(),
            };
            self.exec_mutation(session_id, &ir, false)?
        };
        for abs in &dirs {
            remove_empty_dirs(abs);
        }
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.op = "delete_nodes_found".to_string();
        }
        Ok(result)
    }

    /// `MOVE|COPY NODES FOUND … TO 'dir/'` — relocate every member into one
    /// directory, each keeping its basename.
    ///
    /// Unlike the single-node form the destination cannot be a rename: N sources
    /// cannot share one new name.
    pub(super) fn exec_move_nodes_found_to(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
        is_move: bool,
    ) -> Result<ForgeQLResult> {
        let (dst, if_rev) = match op {
            ForgeQLIR::MoveNodesFoundTo { dst, if_rev } => (dst.as_str(), if_rev.as_deref()),
            ForgeQLIR::CopyNodesFoundTo { dst } => (dst.as_str(), None),
            _ => bail!("exec_move_nodes_found_to called with wrong IR variant"),
        };
        let verb = if is_move { "MOVE" } else { "COPY" };
        // A COPY creates; it cannot destroy what it did not read, so it needs no
        // gate. A MOVE unlinks the sources, so it does.
        let if_rev = if is_move {
            Some(require_found_rev(if_rev, verb)?)
        } else {
            None
        };
        let sid = require_session_id(session_id)?;
        let set = self.armed_found_set(session_id)?;
        let paths = Self::found_set_paths(&set, verb)?;
        self.verify_found_rev(session_id, &set, if_rev)?;

        let dst_dir = self.resolve_last_destination(session_id, dst, verb)?;

        let mut plan = TransformPlan {
            file_edits: Vec::new(),
            suggestions: Vec::new(),
        };
        let mut created_dirs: Vec<PathBuf> = Vec::new();
        {
            let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
            let root = workspace.root().to_path_buf();
            for rel in &paths {
                let src_abs = workspace.safe_path(rel)?;
                if src_abs.is_dir() {
                    bail!(
                        "{verb} NODES FOUND: '{rel}' is a directory — move the files individually, \
                         or arm a set of files"
                    );
                }
                let basename = Path::new(rel).file_name().ok_or_else(|| {
                    anyhow::anyhow!("{verb} NODES FOUND: '{rel}' has no basename")
                })?;
                let dst_rel = Path::new(&dst_dir).join(basename);
                let dst_abs = workspace.safe_path(&dst_rel.to_string_lossy())?;
                if dst_abs == src_abs {
                    bail!("{verb} NODES FOUND: destination is the source ({rel})");
                }
                if dst_abs.exists() {
                    bail!(
                        "{verb} NODES FOUND: destination '{}' already exists — the engine will not \
                         clobber it. DELETE NODE it first, or choose another directory.",
                        dst_rel.display()
                    );
                }
                created_dirs.extend(missing_ancestors(&dst_abs, &root));
                if let Some(parent) = dst_abs.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // A whole-file move is a rename: copy the bytes and unlink the
                // source in the SAME plan, so the file is never briefly in two
                // places or in none.
                #[allow(clippy::naive_bytecount)]
                let line_count = crate::workspace::file_io::read_bytes(&src_abs)
                    .map_or(1, |bytes| {
                        bytes.iter().filter(|b| **b == b'\n').count().max(1)
                    });
                plan.file_edits
                    .extend(plan_copy_lines(rel, &src_abs, 1, line_count, &dst_abs)?.file_edits);
                if is_move {
                    plan.file_edits.push(FileEdit {
                        path: src_abs,
                        edits: Vec::new(),
                        delete: true,
                    });
                }
            }
        }
        self.record_created(sid, &created_dirs);

        let op_name = if is_move {
            "move_nodes_found_to"
        } else {
            "copy_nodes_found_to"
        };
        self.apply_plan(sid, plan, op_name, None)
    }

    /// Where a bulk `TO` points. Always a directory: every member keeps its own
    /// basename, so a file path would name one destination for N sources.
    fn resolve_last_destination(
        &self,
        session_id: Option<&str>,
        dst: &str,
        verb: &str,
    ) -> Result<String> {
        // A handle must resolve to a directory node.
        if let Ok(node) = self.resolve_node(session_id, dst, None) {
            if node.kind != "dir" {
                bail!(
                    "{verb} NODES FOUND: destination '{dst}' is a {} — a set moves into a \
                     directory, so every member can keep its basename",
                    node.kind
                );
            }
            return Ok(node.rel_path);
        }
        // Otherwise it is a path. It does not have to exist yet (that is the one
        // thing a handle cannot express), but it must name a directory.
        let rel = dst.trim_end_matches('/');
        if rel.is_empty() {
            bail!("{verb} NODES FOUND: destination is empty");
        }
        if Path::new(rel).extension().is_some() {
            bail!(
                "{verb} NODES FOUND: destination '{dst}' looks like a file — a set moves into a \
                 directory. Add a trailing '/' if you meant a new directory."
            );
        }
        Ok(rel.to_string())
    }

    /// `INSERT NODE FOR '<path>'` — bring a path into existence and hand back
    /// its handle.
    ///
    /// Every other verb addresses something that already exists; creation is the
    /// one operation a handle cannot express, because the path has no
    /// fingerprint to look up yet. Until now file creation was the undocumented
    /// `COPY LINES 1-1` hack — one real task copied 80 files that way, one call
    /// each.
    ///
    /// A trailing slash creates a **directory** instead. Note that git does not
    /// track empty directories: it exists on disk and `FIND files` lists it, but
    /// it will not survive a commit/clone round-trip until a file lands in it.
    /// The engine does **not** invent a `.gitkeep` — that is a decision for the
    /// agent, not a courtesy from the tool.
    pub(super) fn exec_insert_node_for(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let ForgeQLIR::InsertNodeFor { path } = op else {
            bail!("exec_insert_node_for called with wrong IR variant")
        };
        let sid = require_session_id(session_id)?;
        let is_dir = path.ends_with('/');
        let rel = path.trim_end_matches('/').to_owned();
        if rel.is_empty() {
            bail!("INSERT NODE FOR: empty path");
        }

        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        // safe_path rejects absolute paths, `..` escapes, symlinks out of the
        // worktree, and the `.git` / `.forgeql*` denylist.
        let abs = workspace.safe_path(&rel)?;
        if abs.exists() {
            bail!(
                "INSERT NODE FOR '{path}': already exists — address it by handle instead, \
                 or DELETE NODE it first"
            );
        }

        let root = workspace.root().to_path_buf();
        // Directories brought into existence as a side effect are part of what
        // this command created, and ROLLBACK must remove those and nothing else.
        let mut created = missing_ancestors(&abs, &root);
        if is_dir {
            created.push(abs.clone());
            std::fs::create_dir_all(&abs)?;
        } else {
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::workspace::file_io::write_atomic(&abs, &[])?;
            created.push(abs);
        }

        // Record the creation so ROLLBACK removes it: the path is untracked
        // until COMMIT stages it, so `git reset --hard` would leave it behind.
        self.record_created(sid, &created);
        self.reindex_session(sid, std::slice::from_ref(&PathBuf::from(&rel)));

        let node_id = format!(
            "n{}",
            crate::node_id::hex_prefix(&crate::node_id::sha256_of_path(&rel), 12)
        );
        let mut result = ForgeQLResult::Mutation(crate::result::MutationResult {
            op: "insert_node_for".to_string(),
            applied: true,
            files_changed: vec![PathBuf::from(&rel)],
            edit_count: 1,
            lines_written: 0,
            lines_removed: 0,
            diff: None,
            suggestions: Vec::new(),
            new_node_id: None,
            new_rev: None,
        });
        // Hand back the new path's handle AND its rev: the next command is almost
        // always a write into it, and that write should not need a re-read first.
        self.stamp_new_handle(sid, &mut result, Some(node_id));
        Ok(result)
    }
    pub(super) fn exec_insert_node(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (node_id, before, if_rev, content) = match op {
            ForgeQLIR::InsertNode {
                node_id,
                before,
                if_rev,
                content,
            } => (
                node_id.as_str(),
                *before,
                if_rev.as_deref(),
                content.as_str(),
            ),
            _ => bail!("exec_insert_node called with wrong IR variant"),
        };

        let sid = require_session_id(session_id)?;
        // A whole-file handle is the BOF/EOF append form: it adds lines, it cannot
        // clobber what is already there, so it needs no gate — same reasoning as
        // the creation verbs. A node anchor is different: if it moved since the
        // agent read it, the content lands in the wrong place.
        let anchor_kind = self.node_kind_of(session_id, node_id)?;
        let if_rev = if is_path_kind(&anchor_kind) {
            None
        } else {
            Some(require_rev(if_rev, "INSERT … NODE", node_id)?)
        };
        let node = self.resolve_node(session_id, node_id, if_rev)?;
        // Inserting around a whole-file handle is the BOF/EOF form and needs no
        // guard — it creates, it does not destroy. A directory has no lines to
        // insert around.
        if node.kind == "dir" {
            bail!(
                "INSERT ... NODE '{node_id}' addresses a directory — insert around a \
                 file handle or a node inside one."
            );
        }
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
        let mut result = self.apply_plan(sid, plan, "insert_node", None)?;

        // After reindex, find the first symbol at the insertion line.
        let new_node_id = {
            let session = self.require_session(sid)?;
            session
                .engine_for(&crate::ir::Backend::Default)?
                .find_node_id_at_line(&node.rel_path, insert_line)
        };
        self.stamp_new_handle(sid, &mut result, new_node_id);
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
        let if_rev = Some(require_rev(if_rev, "DELETE NODE", node_id)?);
        let NodeSpan {
            rel_path,
            node_end_line,
            start,
            end,
            has_offset,
            kind,
            ..
        } = self.resolve_node_span(session_id, node_id, if_rev)?;

        // A bare-hex handle addresses a whole file or a whole directory. That is
        // a different operation, not a line span: blanking the lines of a file
        // leaves a 0-byte ghost behind instead of removing it.
        if is_path_kind(&kind) && !has_offset {
            require_path_rev("DELETE", node_id, &kind, if_rev)?;
            return self.delete_path_node(session_id, &rel_path, &kind);
        }
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
        let mut result = self.exec_mutation(session_id, &ir, false)?;
        // The line-delete plumbing reuses ChangeContent, but the agent issued
        // DELETE NODE — report it under its own op name.
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.op = "delete_node".to_string();
        }
        Ok(result)
    }

    /// `MOVE NODE '<src>' IF REV TO '<dst>'` and `COPY NODE '<src>' TO '<dst>'`.
    ///
    /// The anchor form (`… BEFORE|AFTER NODE`) places a node *relative to
    /// another node*. This form places it *at a path* — which is what moving a
    /// file, renaming a file, or lifting a function into a new file all are.
    /// `<dst>` is either a directory handle (the basename is kept) or a path
    /// that does not exist yet (a full rename). Nothing else can be a handle:
    /// a path with nothing at it has no fingerprint to look up.
    ///
    /// A whole-file source is moved as a file: its bytes are written at the
    /// destination and the source is **unlinked**, not left empty. A node-form
    /// source moves just that span. MOVE with a whole-file source is
    /// destructive, so it takes the same mandatory `IF REV` as DELETE; COPY only
    /// creates, so it is ungated.
    pub(super) fn exec_move_node_to(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
        is_move: bool,
    ) -> Result<ForgeQLResult> {
        let (src_id, dst, if_rev) = match op {
            ForgeQLIR::MoveNodeTo {
                src_id,
                dst,
                if_rev,
            } => (src_id.as_str(), dst.as_str(), if_rev.as_deref()),
            ForgeQLIR::CopyNodeTo { src_id, dst } => (src_id.as_str(), dst.as_str(), None),
            _ => bail!("exec_move_node_to called with wrong IR variant"),
        };
        // MOVE unlinks the source, so it is gated. COPY only creates.
        let if_rev = if is_move {
            Some(require_rev(if_rev, "MOVE NODE … TO", src_id)?)
        } else {
            None
        };
        let sid = require_session_id(session_id)?;
        let src = self.resolve_node_span(session_id, src_id, if_rev)?;
        if src.kind == "dir" {
            bail!(
                "MOVE/COPY NODE '{src_id}' addresses a directory — move the files \
                 individually, or address a file inside it"
            );
        }
        let whole_file = src.kind == "file" && !src.has_offset;
        if is_move && whole_file {
            require_path_rev("MOVE", src_id, &src.kind, if_rev)?;
        }

        let dst_rel = self.resolve_move_destination(session_id, &src.rel_path, dst)?;
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let src_abs = workspace.safe_path(&src.rel_path)?;
        let dst_abs = workspace.safe_path(&dst_rel)?;
        if dst_abs == src_abs {
            bail!("MOVE/COPY NODE: destination is the source ({dst_rel})");
        }
        if dst_abs.exists() {
            bail!(
                "MOVE/COPY NODE: destination '{dst_rel}' already exists — the engine will not \
                 clobber it. DELETE NODE it first, or choose another path."
            );
        }
        // Directories the destination needs are created here, so they are part
        // of what this command created — ROLLBACK removes those and nothing
        // else. (The destination file itself is recorded by `apply_plan`, which
        // learns it from `TransformResult.created`.)
        let root = workspace.root().to_path_buf();
        let created_dirs = missing_ancestors(&dst_abs, &root);
        if let Some(parent) = dst_abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        self.record_created(sid, &created_dirs);

        let mut plan = if is_move && !whole_file {
            crate::transforms::copy_move::plan_move_lines(
                &src.rel_path,
                &src_abs,
                src.start,
                src.end,
                &dst_abs,
                None,
            )?
        } else {
            crate::transforms::copy_move::plan_copy_lines(
                &src.rel_path,
                &src_abs,
                src.start,
                src.end,
                &dst_abs,
            )?
        };
        if is_move && whole_file {
            // A whole-file move is a rename: the source file goes, rather than
            // being left behind as a 0-byte husk. Same lowering as a whole-file
            // DELETE, in the same plan — so the file is never briefly in two
            // places or in none.
            plan.file_edits.push(crate::transforms::FileEdit {
                path: src_abs,
                edits: Vec::new(),
                delete: true,
            });
        }

        let op_name = if is_move {
            "move_node_to"
        } else {
            "copy_node_to"
        };
        let mut result = self.apply_plan(sid, plan, op_name, None)?;
        // The handle is path-derived, so the moved/copied file has a new one.
        let moved_id = format!(
            "n{}",
            crate::node_id::hex_prefix(&crate::node_id::sha256_of_path(&dst_rel), 12)
        );
        self.stamp_new_handle(sid, &mut result, Some(moved_id));
        Ok(result)
    }

    /// Where a `TO` argument points.
    ///
    /// A bare-hex handle must resolve to a **directory** (the source keeps its
    /// basename). Anything else is read as a path: a trailing slash or an
    /// existing directory means "into this directory", otherwise it is the full
    /// destination path — the rename.
    fn resolve_move_destination(
        &self,
        session_id: Option<&str>,
        src_rel: &str,
        dst: &str,
    ) -> Result<String> {
        let basename = std::path::Path::new(src_rel)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if crate::storage::path_node::is_handle(dst) {
            let node = self.resolve_node(session_id, dst, None)?;
            if node.kind != "dir" {
                bail!(
                    "MOVE/COPY NODE ... TO '{dst}' resolves to a file. A handle destination must \
                     be a directory; pass a path to rename."
                );
            }
            return Ok(format!("{}/{basename}", node.rel_path));
        }

        let trimmed = dst.trim_end_matches('/');
        if trimmed.is_empty() {
            bail!("MOVE/COPY NODE: empty destination");
        }
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let looks_like_dir = dst.ends_with('/') || workspace.root().join(trimmed).is_dir();
        if looks_like_dir {
            Ok(format!("{trimmed}/{basename}"))
        } else {
            Ok(trimmed.to_owned())
        }
    }
    /// `MOVE NODE 'src' IF REV 'rev' BEFORE|AFTER NODE 'dst'`
    ///
    /// Relocation, not re-authoring: the node's bytes are lifted verbatim and
    /// spliced at the anchor. Delete and insert land in ONE plan, so the file is
    /// never briefly missing the node and a failure leaves nothing half-moved.
    ///
    /// The engine does NOT re-indent (P1). `plan_move_lines` already refuses an
    /// insertion point inside the moved range, which is what makes "move a node
    /// into itself" an error rather than a corruption.
    pub(super) fn exec_move_node(
        &mut self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let (src_id, before, dst_id, if_rev) = match op {
            ForgeQLIR::MoveNode {
                src_id,
                before,
                dst_id,
                if_rev,
            } => (src_id.as_str(), *before, dst_id.as_str(), if_rev.as_deref()),
            _ => bail!("exec_move_node called with wrong IR variant"),
        };
        let if_rev = Some(require_rev(if_rev, "MOVE NODE", src_id)?);

        let sid = require_session_id(session_id)?;
        let src = self.resolve_node_span(session_id, src_id, if_rev)?;
        let dst = self.resolve_node_span(session_id, dst_id, None)?;

        // A bare-hex source moves the whole file: its content is spliced in at
        // the anchor and the file is left empty. That is destructive, so it is
        // gated like a delete. (The empty file is reported, never auto-removed
        // — the engine does not decide that for the agent.) A directory cannot
        // be spliced into a line at all; MOVE NODE ... TO is the verb for that.
        if is_path_kind(&src.kind) && !src.has_offset {
            if src.kind == "dir" {
                bail!(
                    "MOVE NODE '{src_id}' addresses a directory — a directory has no \
                     content to splice. Move the files individually."
                );
            }
            require_path_rev("MOVE", src_id, &src.kind, if_rev)?;
        }
        if dst.kind == "dir" {
            bail!(
                "MOVE NODE ... {} NODE '{dst_id}' addresses a directory — an anchor \
                 must be a file or a node inside one.",
                if before { "BEFORE" } else { "AFTER" }
            );
        }

        // Anchor line, in the file's PRE-move numbering.
        let at = if before { dst.start } else { dst.end + 1 };

        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let src_abs = workspace.safe_path(&src.rel_path)?;
        let dst_abs = workspace.safe_path(&dst.rel_path)?;

        let plan = crate::transforms::copy_move::plan_move_lines(
            &src.rel_path,
            &src_abs,
            src.start,
            src.end,
            &dst_abs,
            Some(at),
        )?;
        let mut result = self.apply_plan(sid, plan, "move_node", None)?;

        // Where the payload came to rest, in the POST-move numbering. Moving a
        // node DOWN inside one file first removes it from above the anchor, so
        // the anchor slides up by the node's height.
        let same_file = src.rel_path == dst.rel_path;
        let moved = src.end.saturating_sub(src.start) + 1;
        let landed = if same_file && src.start < at {
            at.saturating_sub(moved)
        } else {
            at
        };

        // Re-parenting changes parent_ordinal, so the node earns a fresh handle.
        let new_node_id = {
            let session = self.require_session(sid)?;
            session
                .engine_for(&crate::ir::Backend::Default)?
                .find_node_id_at_line(&dst.rel_path, landed)
        };
        self.stamp_new_handle(sid, &mut result, new_node_id);
        Ok(result)
    }

    /// Whole-file / whole-directory delete: unlink, never blank.
    ///
    /// A node delete lowers to `ChangeTarget::Lines { content: "" }`. Applied to
    /// a whole file that would empty it and leave a 0-byte ghost in the index —
    /// the file form has to take the `ChangeTarget::Delete` path instead (the
    /// one `WITH NOTHING` uses), which unlinks the file, keeps the original for
    /// ROLLBACK, and stages the removal at COMMIT.
    ///
    /// A directory is the same operation over every file underneath it: one
    /// plan, so the whole subtree lands or none of it does. The now-empty
    /// directories are removed bottom-up afterwards — `remove_dir` refuses a
    /// non-empty directory, so anything the walk could not see (an ignored file)
    /// keeps its parent alive rather than being silently destroyed.
    fn delete_path_node(
        &mut self,
        session_id: Option<&str>,
        rel_path: &str,
        kind: &str,
    ) -> Result<ForgeQLResult> {
        let is_dir = kind == "dir";
        let (workspace, _engine) = self.require_workspace_and_engine(session_id)?;
        let root = workspace.root().to_path_buf();
        let abs = workspace.safe_path(rel_path)?;

        let files: Vec<String> = if is_dir {
            workspace
                .files()
                .filter(|p| !crate::result::FileEntry::is_runtime_artifact(p))
                .filter(|p| p.starts_with(&abs))
                .map(|p| {
                    p.strip_prefix(&root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .into_owned()
                })
                .collect()
        } else {
            vec![rel_path.to_owned()]
        };

        let mut result = if files.is_empty() {
            // An empty directory: nothing to unlink, only the directory itself.
            ForgeQLResult::Mutation(crate::result::MutationResult {
                op: "delete_node".to_string(),
                applied: true,
                files_changed: Vec::new(),
                edit_count: 0,
                lines_written: 0,
                lines_removed: 0,
                diff: None,
                suggestions: Vec::new(),
                new_node_id: None,
                new_rev: None,
            })
        } else {
            let ir = ForgeQLIR::ChangeContent {
                files,
                target: ChangeTarget::Delete,
                clauses: crate::ir::Clauses::default(),
            };
            self.exec_mutation(session_id, &ir, false)?
        };

        if is_dir {
            remove_empty_dirs(&abs);
        }
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.op = "delete_node".to_string();
        }
        Ok(result)
    }

    /// The `fql_kind` of a handle, without resolving its span or checking a rev.
    ///
    /// Used to decide whether a verb needs the gate at all: a whole-file or
    /// directory handle behaves differently from a node inside one.
    fn node_kind_of(&self, session_id: Option<&str>, node_id: &str) -> Result<String> {
        let session = self.require_session(require_session_id(session_id)?)?;
        let root = session.worktree_path.clone();
        Ok(session
            .engine_for(&crate::ir::Backend::Default)?
            .find_node(node_id, &root)?
            .map(|n| n.fql_kind)
            .unwrap_or_default())
    }

    /// Resolve `node_id` → (`rel_path`, `line`, `end_line`, `kind`) and optionally check IF REV guard.
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
            kind: node.fql_kind,
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
            kind: node.kind,
        })
    }
}

/// Scratch struct for resolved node location used by Phase C mutation helpers.
struct ResolvedNode {
    rel_path: String,
    line: usize,
    end_line: usize,
    /// `fql_kind` of the resolved node. `file` and `dir` mark the synthesized
    /// whole-path nodes a bare-hex handle resolves to — the mutation verbs treat
    /// those differently (unlink rather than blank; mandatory `IF REV`).
    kind: String,
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
    /// `fql_kind` of the base node — see [`ResolvedNode::kind`].
    kind: String,
}

/// Is this the synthesized node of a whole file or a whole directory (a bare-hex
/// `n<hex>` handle)?
fn is_path_kind(kind: &str) -> bool {
    kind == "file" || kind == "dir"
}

/// Destructive whole-path mutations require `IF REV`.
///
/// A node edit can be reviewed and corrected afterwards; deleting a file or
/// overwriting all of it leaves nothing to re-read. The rev is the agent
/// proving it is acting on what it actually saw — for a directory that is its
/// membership (the files it listed), for a file its bytes.
fn require_path_rev(op: &str, node_id: &str, kind: &str, if_rev: Option<&str>) -> Result<()> {
    if if_rev.is_none() && is_path_kind(kind) {
        bail!(
            "whole-{kind} {op} requires IF REV — read the current rev with \
             FIND NODE '{node_id}' (or FIND files) and repeat the command with \
             IF REV '<rev>'"
        );
    }
    Ok(())
}

/// Every verb that names an **existing** node takes `IF REV`.
///
/// Not safety theatre: an agent may carry a handle across dozens of commands and
/// then come back to it. The handle still resolves — handles are stable — but the
/// code under it may have moved, including under an edit to one of its *children*,
/// which changes the enclosing node's rev too (a rev is the hash of the node's
/// whole span). Nothing else can tell the agent that the thing it remembers is no
/// longer the thing that is there.
///
/// Creation verbs (`INSERT NODE FOR`, `COPY NODE … TO`) are exempt: a path that
/// does not exist yet has nothing to fingerprint.
fn require_rev<'a>(if_rev: Option<&'a str>, verb: &str, node_id: &str) -> Result<&'a str> {
    if_rev.ok_or_else(|| {
        anyhow::anyhow!(
            "{verb} requires IF REV '<rev>'. The rev travels with the handle: it is on the \
             FIND / SHOW row that handed you '{node_id}', and on the result of the mutation \
             that last touched it. If you no longer have it, FIND NODE '{node_id}' returns it."
        )
    })
}

/// The bulk FOUND verbs that destroy require `IF REV`.
///
/// The grammar accepts the clause as optional so that a missing one lands here
/// rather than falling through to the single-node verb, which would report an
/// `invalid node_id: FOUND` — an error about the wrong thing entirely.
fn require_found_rev<'a>(if_rev: Option<&'a str>, verb: &str) -> Result<&'a str> {
    if_rev.ok_or_else(|| {
        anyhow::anyhow!(
            "{verb} NODES FOUND requires IF REV '<master rev>' — it acts on every member of the \
             set at once. Re-run the FIND: its response carries the master rev to quote here."
        )
    })
}

/// Remove `dir` and every directory under it that is empty, deepest first.
///
/// `remove_dir` refuses a non-empty directory, so anything the file walk could
/// not see — an ignored build artifact, say — keeps its parent alive instead of
/// being destroyed silently.
fn remove_empty_dirs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_empty_dirs(&entry.path());
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// The ancestors of `abs` that do not exist yet, shallowest first, stopping at
/// the worktree root.
///
/// A creation verb that calls `create_dir_all` brings these into existence as a
/// side effect. ROLLBACK has to remove exactly what the transaction created and
/// nothing else — an empty directory that was already there is not the engine's
/// to delete, and git will not restore it (git does not track empty directories),
/// so guessing wrong is unrecoverable.
fn missing_ancestors(abs: &std::path::Path, root: &std::path::Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut cur = abs.parent();
    while let Some(dir) = cur {
        if dir == root || !dir.starts_with(root) || dir.exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        cur = dir.parent();
    }
    missing.reverse();
    missing
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
