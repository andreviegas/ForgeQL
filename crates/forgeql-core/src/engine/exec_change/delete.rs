//! `DELETE NODE` — node spans, offset sub-ranges, and whole-path (file or
//! directory) deletion with its mandatory `IF REV` guard.

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::ir::{ChangeTarget, ForgeQLIR};
use crate::result::ForgeQLResult;

use crate::transforms::change::absorb_trailing_blank_lines;

use super::resolve::{NodeSpan, is_path_kind, require_path_rev, require_rev};

impl ForgeQLEngine {
    pub(in crate::engine) fn exec_delete_node(
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
        // Stage a tombstone for the removed root ordinal so the reindex
        // this delete triggers keeps a byte-identical surviving sibling on its
        // own ordinal instead of letting it silently adopt the deleted handle.
        // Only a whole-node delete frees an ordinal — a line-range delete
        // (`has_offset`) keeps the node, so it must not tombstone.
        if !has_offset
            && let Ok((base_id, _)) = crate::node_id::split_node_offset(node_id)
            && let Some(ordinal) = crate::node_id::ordinal_of(base_id)
            && let Ok(sid) = require_session_id(session_id)
            && let Some(session) = self.sessions.get_mut(sid)
        {
            session
                .pending_tombstones
                .entry(rel_path.clone().into())
                .or_default()
                .push(ordinal);
        }
        let ir = ForgeQLIR::ChangeContent {
            files: vec![rel_path],
            target: ChangeTarget::Lines {
                start,
                end,
                content: String::new(),
            },
            clauses: crate::ir::Clauses::default(),
        };
        let result = self.exec_mutation(session_id, &ir, false);
        // If the mutation failed, no reindex ran and the staged tombstone was
        // never consumed — drop it, or it would wrongly re-key a still-present
        // node on a later, unrelated edit to the same file.
        if result.is_err()
            && let Ok(sid) = require_session_id(session_id)
            && let Some(session) = self.sessions.get_mut(sid)
        {
            session.pending_tombstones.clear();
        }
        let mut result = result?;
        // The line-delete plumbing reuses ChangeContent, but the agent issued
        // DELETE NODE — report it under its own op name.
        if let ForgeQLResult::Mutation(ref mut m) = result {
            m.op = "delete_node".to_string();
        }
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
                structural_errors: Vec::new(),
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
}
/// Remove `dir` and every directory under it that is empty, deepest first.
///
/// `remove_dir` refuses a non-empty directory, so anything the file walk could
/// not see — an ignored build artifact, say — keeps its parent alive instead of
/// being destroyed silently.
pub(super) fn remove_empty_dirs(dir: &std::path::Path) {
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
