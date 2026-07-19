//! Handle resolution shared by every mutation verb: node → file/span lookup,
//! offset-suffix spans, `IF REV` guards, and the self-healing rev-mismatch
//! rejection payload.

use anyhow::{Result, bail};

use crate::engine::{ForgeQLEngine, require_session_id};
use crate::error::{ForgeError, RejectionKind};

impl ForgeQLEngine {
    /// The `fql_kind` of a handle, without resolving its span or checking a rev.
    ///
    /// Used to decide whether a verb needs the gate at all: a whole-file or
    /// directory handle behaves differently from a node inside one.
    pub(super) fn node_kind_of(&self, session_id: Option<&str>, node_id: &str) -> Result<String> {
        let session = self.require_session(require_session_id(session_id)?)?;
        let root = session.worktree_path.clone();
        Ok(session
            .engine_for(&crate::ir::Backend::Default)?
            .find_node(node_id, &root)?
            .map(|n| n.fql_kind)
            .unwrap_or_default())
    }
    /// Resolve `node_id` → (`rel_path`, `line`, `end_line`, `kind`) and optionally check IF REV guard.
    pub(super) fn resolve_node(
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
            .ok_or_else(|| ForgeError::Rejection {
                kind: RejectionKind::NodeNotFound,
                payload: format!(r#"{{"error":"node_not_found","node_id":"{node_id}"}}"#),
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
            return Err(ForgeError::Rejection {
                kind: RejectionKind::RevMismatch,
                payload: payload.to_string(),
            }
            .into());
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
    pub(super) fn resolve_node_span(
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
pub(super) struct ResolvedNode {
    pub(super) rel_path: String,
    pub(super) line: usize,
    pub(super) end_line: usize,
    /// `fql_kind` of the resolved node. `file` and `dir` mark the synthesized
    /// whole-path nodes a bare-hex handle resolves to — the mutation verbs treat
    /// those differently (unlink rather than blank; mandatory `IF REV`).
    pub(super) kind: String,
}
/// A node resolved to the line span an operation targets, honoring an optional
/// `(n-m)` offset suffix. Shared by CHANGE NODE and DELETE NODE so offset
/// addressing is defined in exactly one place.
pub(super) struct NodeSpan {
    pub(super) rel_path: String,
    /// Whole-node last line — used for trailing-blank absorption on a whole delete.
    pub(super) node_end_line: usize,
    /// 1-based inclusive target span: the whole node, or the offset sub-range.
    pub(super) start: usize,
    pub(super) end: usize,
    /// True when an `(n-m)` suffix narrowed the span to a sub-range.
    pub(super) has_offset: bool,
    /// 1-based start line of the base node, used to re-resolve the post-edit
    /// handle by position so the caller learns the new id even if it churned.
    pub(super) node_line: usize,
    /// `fql_kind` of the base node — see [`ResolvedNode::kind`].
    pub(super) kind: String,
}
/// Is this the synthesized node of a whole file or a whole directory (a bare-hex
/// `n<hex>` handle)?
pub(super) fn is_path_kind(kind: &str) -> bool {
    kind == "file" || kind == "dir"
}
/// Destructive whole-path mutations require `IF REV`.
///
/// A node edit can be reviewed and corrected afterwards; deleting a file or
/// overwriting all of it leaves nothing to re-read. The rev is the agent
/// proving it is acting on what it actually saw — for a directory that is its
/// membership (the files it listed), for a file its bytes.
pub(super) fn require_path_rev(
    op: &str,
    node_id: &str,
    kind: &str,
    if_rev: Option<&str>,
) -> Result<()> {
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
pub(super) fn require_rev<'a>(
    if_rev: Option<&'a str>,
    verb: &str,
    node_id: &str,
) -> Result<&'a str> {
    if_rev.ok_or_else(|| {
        anyhow::anyhow!(
            "{verb} requires IF REV '<rev>'. The rev travels with the handle: it is on the \
             FIND / SHOW row that handed you '{node_id}', and on the result of the mutation \
             that last touched it. If you no longer have it, FIND NODE '{node_id}' returns it."
        )
    })
}
/// Extract the inclusive 1-based line span `[line_start, line_end]` from `src`.
fn node_span_text(src: &str, line_start: usize, line_end: usize) -> String {
    src.lines()
        .skip(line_start.saturating_sub(1))
        .take(line_end.saturating_sub(line_start).saturating_add(1))
        .collect::<Vec<_>>()
        .join("\n")
}
/// Line budget for the `current_content` echoed back in a `rev_mismatch`
/// payload. Seeing a stale *statement* in full is invaluable for re-targeting;
/// echoing a stale whole *file* buries the error under thousands of tokens.
/// Past this many lines the body is elided to a head and a tail, with a note
/// pointing at SHOW NODE for the complete text.
const REV_MISMATCH_CONTENT_MAX_LINES: usize = 40;
const REV_MISMATCH_CONTENT_HEAD_LINES: usize = 24;
const REV_MISMATCH_CONTENT_TAIL_LINES: usize = 8;

/// Elide an oversized node body for a `rev_mismatch` error, keeping the head
/// and tail and noting how many lines were dropped. Content within the budget
/// is returned verbatim, so small nodes are unaffected.
fn cap_rev_mismatch_content(node_id: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= REV_MISMATCH_CONTENT_MAX_LINES {
        return content.to_string();
    }
    let head = lines[..REV_MISMATCH_CONTENT_HEAD_LINES].join("\n");
    let tail = lines[lines.len() - REV_MISMATCH_CONTENT_TAIL_LINES..].join("\n");
    let elided = lines.len() - REV_MISMATCH_CONTENT_HEAD_LINES - REV_MISMATCH_CONTENT_TAIL_LINES;
    format!(
        "{head}\n… {elided} lines elided ({total} total) — SHOW NODE '{node_id}' for the full text …\n{tail}",
        total = lines.len(),
    )
}

/// Build the self-healing rejection payload for a failed `CHANGE NODE … IF REV`
/// guard. Carries the node's current rev, line range, and source so the agent
/// can re-target the edit without a follow-up read.
pub(super) fn rev_mismatch_payload(
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
        "current_content": cap_rev_mismatch_content(node_id, current_content),
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

    #[test]
    fn rev_mismatch_payload_caps_oversized_content() {
        // A small node is echoed verbatim.
        let small = "line1\nline2\nline3";
        let payload = rev_mismatch_payload("nabc.0000", "hexp", "hcur", 1, 3, small);
        assert_eq!(payload["current_content"], small);

        // A large (file-sized) node is elided head+tail with a pointer to SHOW.
        let big: String = (1..=200)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let payload = rev_mismatch_payload("nabc.0000", "hexp", "hcur", 1, 200, &big);
        let content = payload["current_content"].as_str().unwrap();
        let lines: Vec<&str> = content.lines().collect();
        // head + note + tail, far short of the 200 original lines.
        assert_eq!(
            lines.len(),
            super::REV_MISMATCH_CONTENT_HEAD_LINES + 1 + super::REV_MISMATCH_CONTENT_TAIL_LINES
        );
        assert_eq!(lines[0], "line1");
        assert_eq!(*lines.last().unwrap(), "line200");
        assert!(content.contains("SHOW NODE 'nabc.0000'"));
        assert!(content.contains("168 lines elided"));
    }
}
