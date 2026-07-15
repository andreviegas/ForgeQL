//! `INSERT NODE FOR '<path>'` (bring a new file or directory into existence)
//! and `INSERT BEFORE / AFTER NODE` (splice text around an existing node).

use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::ir::ForgeQLIR;
use crate::result::ForgeQLResult;
use crate::transforms::change::lines_to_byte_range;
use crate::transforms::{ByteRangeEdit, FileEdit, TransformPlan};

use super::plan::missing_ancestors;
use super::resolve::{is_path_kind, require_rev};

impl ForgeQLEngine {
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
    pub(in crate::engine) fn exec_insert_node_for(
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
            structural_errors: Vec::new(),
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
    pub(in crate::engine) fn exec_insert_node(
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
}
