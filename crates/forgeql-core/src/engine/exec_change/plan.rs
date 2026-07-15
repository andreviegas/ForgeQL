//! The shared mutation pipeline: plan → gate → apply → reindex → diff, the
//! per-session UNDO ring, and created-path bookkeeping for ROLLBACK.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::result::{ForgeQLResult, MutationResult};
use crate::transforms::diff::{CompactDiffConfig, compact_diff_addressed};
use crate::transforms::{FileEdit, TransformPlan};

impl ForgeQLEngine {
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
    pub(super) fn record_created(&mut self, sid: &str, created: &[PathBuf]) {
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
    pub(super) fn apply_plan(
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
        let structural_before = self.structural_validity(sid, &files_changed);
        let applied = plan.apply()?;
        self.reindex_session(sid, &files_changed);
        let structural_errors = self.structural_errors(sid, &files_changed, &structural_before);

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
            structural_errors,
        }))
    }

    /// Read `path` from the worktree and run its language's strict validator, if
    /// it has one. `None` — no validator for this file (or dialect), or the file
    /// could not be read; `Some(result)` — the strict well-formedness verdict.
    fn validate_file(
        &self,
        root: &std::path::Path,
        path: &std::path::Path,
    ) -> Option<Result<(), String>> {
        let lang = self.lang_registry.language_for_path(path)?;
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        let bytes = std::fs::read(&abs).ok()?;
        lang.validate_source(&bytes, path)
    }

    /// Strict validity of each touched file that has a format validator, read
    /// from disk as it stands now. Snapshots the pre-edit state so a later
    /// [`Self::structural_errors`] can report whether the edit caused a break.
    pub(super) fn structural_validity(
        &self,
        sid: &str,
        paths: &[PathBuf],
    ) -> std::collections::HashMap<PathBuf, bool> {
        let mut out = std::collections::HashMap::new();
        let Ok((workspace, _engine)) = self.require_workspace_and_engine(Some(sid)) else {
            return out;
        };
        for path in paths {
            if let Some(result) = self.validate_file(workspace.root(), path) {
                let _ = out.insert(path.clone(), result.is_ok());
            }
        }
        out
    }

    /// After an edit, one [`crate::result::StructuralError`] per touched file a
    /// strict format validator now rejects. `before` is the pre-edit snapshot
    /// from [`Self::structural_validity`], used to report whether this edit
    /// introduced the break. Files without a validator, or that still parse, add
    /// nothing.
    pub(super) fn structural_errors(
        &self,
        sid: &str,
        paths: &[PathBuf],
        before: &std::collections::HashMap<PathBuf, bool>,
    ) -> Vec<crate::result::StructuralError> {
        let mut out = Vec::new();
        let Ok((workspace, _engine)) = self.require_workspace_and_engine(Some(sid)) else {
            return out;
        };
        for path in paths {
            if let Some(Err(message)) = self.validate_file(workspace.root(), path) {
                out.push(crate::result::StructuralError {
                    path: path.clone(),
                    valid_before: before.get(path).copied(),
                    message,
                });
            }
        }
        out
    }

    /// Stamp the post-edit handle **and its rev** onto a mutation result.
    ///
    /// The two always travel together. With `IF REV` mandatory, handing back a
    /// handle without its new rev would make every chained edit on the same node
    /// pay for a `FIND NODE` round trip first.
    pub(super) fn stamp_new_handle(
        &self,
        sid: &str,
        result: &mut ForgeQLResult,
        node_id: Option<String>,
    ) {
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
    /// Restore the files a recent mutation changed to their pre-edit bytes,
    /// reading the per-session UNDO ring (`last` = slot, 0 = most recent).
    ///
    /// Mechanical and language-agnostic: it rewrites the exact bytes `apply()`
    /// captured before the edit, reindexes the restored files, and invalidates
    /// the commit gate like any other mutation. `UNDO LAST-n` restores the slot
    /// `n` back, reversing the `n + 1` most recent mutations at once.
    pub(in crate::engine) fn exec_undo(
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
            structural_errors: Vec::new(),
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
    pub(super) fn build_post_edit_diff(
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
}
/// The ancestors of `abs` that do not exist yet, shallowest first, stopping at
/// the worktree root.
///
/// A creation verb that calls `create_dir_all` brings these into existence as a
/// side effect. ROLLBACK has to remove exactly what the transaction created and
/// nothing else — an empty directory that was already there is not the engine's
/// to delete, and git will not restore it (git does not track empty directories),
/// so guessing wrong is unrecoverable.
pub(super) fn missing_ancestors(abs: &std::path::Path, root: &std::path::Path) -> Vec<PathBuf> {
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
