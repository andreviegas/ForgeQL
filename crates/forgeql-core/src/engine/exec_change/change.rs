//! `CHANGE NODE` — whole-node / offset-range replacement and the
//! `MATCHING 'a' WITH 'b'` textual sweep, both gated by `IF REV`.

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::ir::{ChangeTarget, ForgeQLIR};
use crate::result::ForgeQLResult;
use crate::transforms::change::lines_to_byte_range;
use crate::transforms::{FileEdit, TransformPlan};

use super::resolve::{NodeSpan, is_path_kind, require_path_rev, require_rev};

impl ForgeQLEngine {
    pub(in crate::engine) fn exec_change_node(
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

        // `CHANGE ... WITH ''` empties the addressed span, which removes the
        // construct — exactly like deleting it (WITH '' is the delete form of
        // CHANGE). Retire the handle of every construct fully inside the
        // replaced range, or a byte-identical sibling re-claims the emptied
        // construct's ordinal on the reindex and its dead handle silently
        // repoints to that sibling.
        if content.is_empty() {
            self.stage_removed_span(session_id, &rel_path, start, end)?;
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
    pub(in crate::engine) fn exec_change_node_matching(
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
}
