//! `SHOW MORE` — paging the window a capped read left behind.
//!
//! A read that hits the line cap buffers the remainder rather than discarding
//! it. This walks that buffer, with its own `HEAD`/`TAIL`/range selection and
//! its own `WHERE text` filter, so paging costs nothing until it is asked for.

use anyhow::Result;

use crate::{
    engine::{ForgeQLEngine, require_session_id},
    ir::ForgeQLIR,
    result::{ForgeQLResult, ShowContent, ShowResult, SourceLine},
};

impl ForgeQLEngine {
    /// `SHOW MORE [HEAD n | TAIL n | n-m] [WHERE …] [LIMIT n]`
    ///
    /// Pages the session's last buffered output (`.forgeql-showmore`). The
    /// positional window is applied first, then `WHERE text` predicates and
    /// `LIMIT`/`OFFSET` reuse the same line machinery as `SHOW LINES`. Each
    /// returned line keeps its original buffer index so a precise follow-up
    /// range can be requested. `SHOW MORE` is an explicit retrieval and is
    /// never re-blocked by the inline cap.
    pub(in crate::engine) fn exec_show_more(
        &self,
        session_id: Option<&str>,
        op: &ForgeQLIR,
    ) -> Result<ForgeQLResult> {
        let ForgeQLIR::ShowMore {
            window,
            last,
            clauses,
        } = op
        else {
            unreachable!("exec_show_more: wrong IR variant")
        };
        let sid = require_session_id(session_id)?;
        let root = self.require_session(sid)?.worktree_path.clone();

        // Before the buffer is read, not after: a clause naming a field a
        // source line cannot carry is unanswerable whether or not this session
        // has paged anything yet, and reporting the missing buffer instead
        // hides it until the day someone happens to have one.
        crate::filter::reject_unresolvable_fields::<SourceLine>("SHOW MORE", clauses)?;
        Self::reject_line_shaping("SHOW MORE", clauses)?;
        Self::reject_globs("SHOW MORE", clauses)?;
        crate::filter::reject_depth("SHOW MORE", clauses)?;

        let buffer = crate::showmore::read_buffer_n(&root, *last)
            .map_err(|e| anyhow::anyhow!("reading SHOW MORE buffer: {e}"))?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no SHOW MORE buffer for this session yet — run a command whose \
                     output was truncated, then SHOW MORE to page the rest"
                )
            })?;

        let selection = match *window {
            crate::ir::ShowMoreWindow::Full => crate::showmore::Selection::Full,
            crate::ir::ShowMoreWindow::Head(n) => crate::showmore::Selection::Head(n),
            crate::ir::ShowMoreWindow::Tail(n) => crate::showmore::Selection::Tail(n),
            crate::ir::ShowMoreWindow::Range(a, b) => crate::showmore::Selection::Range(a, b),
        };

        let total = buffer.total();
        let mut lines: Vec<SourceLine> = buffer
            .window(selection)
            .into_iter()
            .map(|(idx, text)| SourceLine {
                rev: None,
                line: idx,
                text: text.to_string(),
                marker: None,
                node_id: None,
                node_offset: None,
            })
            .collect();

        // WHERE text predicates filter the windowed lines — free grep over the
        // buffered output (e.g. `SHOW MORE WHERE text MATCHES 'error|fail'`).
        //
        // These lines came out of a buffer, so nothing here resolves a symbol
        // and the row shape IS the universe — which is why the whole clause was
        // refusable above, before the buffer was even read.
        for predicate in &clauses.where_predicates {
            let pred = predicate.clone();
            lines.retain(|line| crate::filter::eval_predicate(line, &pred));
        }

        let mut show_result = ShowResult {
            op: "show_more".to_string(),
            symbol: Some(buffer.label.clone()),
            file: None,
            content: ShowContent::Lines {
                lines,
                byte_start: None,
                depth: None,
            },
            start_line: None,
            end_line: None,
            total_lines: None,
            hint: None,
            metadata: None,
        };

        // Honour an explicit LIMIT/OFFSET on the buffered lines. SHOW MORE is
        // itself the cap-bypass path, so no inline cap is applied here.
        Self::apply_show_lines_cap(&mut show_result, Some(clauses), None);

        // Tell the agent how much of the buffer it is seeing, so it can page on.
        let shown = match &show_result.content {
            ShowContent::Lines { lines, .. } => lines.len(),
            _ => 0,
        };
        if shown < total {
            show_result.total_lines = Some(total);
            show_result.hint = Some(format!(
                "buffer '{}' has {total} lines; showing {shown}. \
                 Page with SHOW MORE HEAD n | TAIL n | n-m, or filter with \
                 SHOW MORE WHERE text MATCHES '…'.",
                buffer.label
            ));
        }

        // The buffered window holds already-rendered response text. Hand it back
        // as a paged block so the CSV writer emits it verbatim instead of
        // re-quoting (double-encoding) every field. WHERE filtering and the
        // LIMIT/OFFSET cap above already ran on the structured lines.
        let paged: Option<Vec<String>> = match &mut show_result.content {
            ShowContent::Lines { lines, .. } => {
                Some(std::mem::take(lines).into_iter().map(|l| l.text).collect())
            }
            _ => None,
        };
        if let Some(lines) = paged {
            show_result.content = ShowContent::Paged { lines };
        }

        Ok(ForgeQLResult::Show(show_result))
    }
}
