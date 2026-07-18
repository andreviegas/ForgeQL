//! `MOVE NODE` — byte-exact relocation before/after an anchor node — and
//! `MOVE / COPY NODE … TO` (rename, move into a directory, copy).

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::ir::ForgeQLIR;
use crate::result::ForgeQLResult;
use crate::transforms::change::absorb_trailing_blank_lines;

use super::plan::missing_ancestors;
use super::resolve::{is_path_kind, require_path_rev, require_rev};

impl ForgeQLEngine {
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
    pub(in crate::engine) fn exec_move_node_to(
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

        // Whole-node moves absorb the trailing blank separator on the source
        // side, exactly like DELETE NODE — the payload stays the node's exact
        // span. Offset sub-ranges are line-addressed and stay exact.
        let delete_end = if !is_move || whole_file || src.has_offset {
            src.end
        } else {
            std::fs::read_to_string(&src_abs)
                .map(|content| absorb_trailing_blank_lines(&content, src.end))
                .unwrap_or(src.end)
        };
        let mut plan = if is_move && !whole_file {
            crate::transforms::copy_move::plan_move_lines(
                &src.rel_path,
                &src_abs,
                src.start,
                src.end,
                delete_end,
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
    pub(in crate::engine) fn exec_move_node(
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

        // Whole-node moves absorb the trailing blank separator on the source
        // side, exactly like DELETE NODE — the payload stays the node's exact
        // span. Offset sub-ranges are line-addressed and stay exact.
        let delete_end = if src.has_offset {
            src.end
        } else {
            std::fs::read_to_string(&src_abs)
                .map(|content| absorb_trailing_blank_lines(&content, src.end))
                .unwrap_or(src.end)
        };
        let plan = crate::transforms::copy_move::plan_move_lines(
            &src.rel_path,
            &src_abs,
            src.start,
            src.end,
            delete_end,
            &dst_abs,
            Some(at),
        )?;
        // Moving a whole node out of its source file frees that node's
        // ordinal there. Tombstone it so a byte-identical surviving sibling in
        // the source file keeps its own ordinal instead of adopting the moved
        // node's handle. A bare-hex whole-file source or a line-range move frees
        // no per-node ordinal, so it does not tombstone.
        if !src.has_offset
            && !is_path_kind(&src.kind)
            && let Ok((base_id, _)) = crate::node_id::split_node_offset(src_id)
            && let Some(ordinal) = crate::node_id::ordinal_of(base_id)
            && let Some(session) = self.sessions.get_mut(sid)
        {
            session
                .pending_tombstones
                .entry(src.rel_path.clone().into())
                .or_default()
                .push(ordinal);
        }
        let mut result = self.apply_plan(sid, plan, "move_node", None)?;

        // Where the payload came to rest, in the POST-move numbering. Moving a
        // node DOWN inside one file first removes it from above the anchor, so
        // the anchor slides up by the removed span's height (node + absorbed
        // trailing blanks).
        let same_file = src.rel_path == dst.rel_path;
        let removed = delete_end.saturating_sub(src.start) + 1;
        let landed = if same_file && src.start < at {
            at.saturating_sub(removed)
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
}
