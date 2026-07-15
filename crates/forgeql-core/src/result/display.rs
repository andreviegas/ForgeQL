//! `fmt::Display` implementations for all `ForgeQL` result types.
use super::{
    BeginTransactionResult, CommitResult, FileEntry, ForgeQLResult, MutationResult, PlanResult,
    QueryResult, RollbackResult, RunResult, ShowContent, ShowResult, SourceOpResult,
    VerifyBuildResult,
};
use std::fmt;

/// One `SHOW outline` / `SHOW members` row: `line | kind | text | <id> <rev>`.
///
/// The handle and its rev are printed together or not at all — a handle without
/// its rev is an address the agent cannot act on, since `IF REV` is mandatory.
fn write_tree_row(
    formatter: &mut fmt::Formatter<'_>,
    line: usize,
    kind: &str,
    text: &str,
    node_id: Option<&str>,
    rev: Option<&str>,
) -> fmt::Result {
    let suffix = match (node_id, rev) {
        (Some(id), Some(rev)) => format!(" | {id} {rev}"),
        (Some(id), None) => format!(" | {id}"),
        _ => String::new(),
    };
    writeln!(formatter, "{line:>4} | {kind:12} | {text}{suffix}")
}

/// Render a `FIND files` listing: one row per entry, then the total and the
/// master rev that arms a FOUND mutation.
fn write_file_list(
    formatter: &mut fmt::Formatter<'_>,
    files: &[FileEntry],
    total: usize,
    metadata: Option<&serde_json::Map<String, serde_json::Value>>,
) -> fmt::Result {
    for entry in files {
        write!(formatter, "  {}", entry.path.display())?;
        if let Some(ref node_id) = entry.node_id {
            write!(formatter, "  {node_id}")?;
        }
        if let Some(ref rev) = entry.rev {
            write!(formatter, "  {rev}")?;
        }
        writeln!(formatter)?;
    }
    writeln!(formatter, "({total} files)")?;
    // Same master-rev line as FIND symbols/usages — FIND files arms FOUND too,
    // and the gate has to be quotable from here.
    if let Some(rev) = metadata
        .and_then(|m| m.get("found_rev"))
        .and_then(serde_json::Value::as_str)
    {
        writeln!(formatter, "found_rev: {rev}")?;
    }
    Ok(())
}

// -----------------------------------------------------------------------
// Display formatting (human-readable output)
// -----------------------------------------------------------------------
impl fmt::Display for ForgeQLResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(result) => write!(formatter, "{result}"),
            Self::Show(result) => write!(formatter, "{result}"),
            Self::Mutation(result) => write!(formatter, "{result}"),
            Self::SourceOp(result) => write!(formatter, "{result}"),
            Self::BeginTransaction(result) => write!(formatter, "{result}"),
            Self::Commit(result) => write!(formatter, "{result}"),
            Self::Plan(result) => write!(formatter, "{result}"),
            Self::Rollback(result) => write!(formatter, "{result}"),
            Self::VerifyBuild(result) => write!(formatter, "{result}"),
            Self::Run(result) => write!(formatter, "{result}"),
            Self::ExportPatch(r) => {
                writeln!(
                    formatter,
                    "export_patch {} — {} file(s)",
                    r.range,
                    r.files.len()
                )?;
                for f in &r.files {
                    writeln!(
                        formatter,
                        "  {} ({} bytes, sha256 {})",
                        f.path.display(),
                        f.bytes,
                        f.sha256
                    )?;
                }
                if let Some(hint) = &r.hint {
                    writeln!(formatter, "  hint: {hint}")?;
                }
                write!(formatter, "{}", r.content)
            }
            Self::FindNode(r) => write!(
                formatter,
                "find_node {}\n  {} {} {}:{}-{} rev={}",
                r.node_id,
                r.fql_kind,
                r.name,
                r.path.display(),
                r.line,
                r.end_line,
                r.rev
            ),
            Self::JobStarted(r) => {
                write!(formatter, "job {} started — {}", r.id, r.label)
            }
            Self::JobStatus(s) => write!(
                formatter,
                "job {} {} ({} ms)\n{}",
                s.id,
                s.state.as_str(),
                s.elapsed_ms,
                s.output
            ),
            Self::JobList(l) => {
                for job in &l.jobs {
                    writeln!(
                        formatter,
                        "{} {} {} ({} ms)",
                        job.id,
                        job.state.as_str(),
                        job.label,
                        job.elapsed_ms
                    )?;
                }
                Ok(())
            }
            Self::PendingExec(p) => {
                write!(formatter, "job {} started — {}", p.job_id, p.step)
            }
            Self::ShowDiff(d) => {
                writeln!(formatter, "show_diff — {} file(s)", d.files.len())?;
                for f in &d.files {
                    writeln!(
                        formatter,
                        "  {} {} +{} -{}",
                        f.status,
                        f.path.display(),
                        f.added,
                        f.removed
                    )?;
                }
                if let Some(hint) = &d.hint {
                    writeln!(formatter, "  hint: {hint}")?;
                }
                write!(formatter, "{}", d.content)
            }
        }
    }
}

impl fmt::Display for QueryResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.results.is_empty() {
            return writeln!(formatter, "No results.");
        }
        let rows = self.projected_rows();
        for sr in &rows {
            write!(formatter, "{}", sr.name)?;
            if !sr.kind.is_empty() {
                write!(formatter, " | {}", sr.kind)?;
            }
            if !sr.path.is_empty() {
                write!(formatter, " | {}:{}", sr.path, sr.line)?;
            }
            if let Some(ref fn_name) = sr.enclosing_fn {
                write!(formatter, " | via {fn_name}")?;
            }
            // The handle and its rev, together — everything a mutation needs.
            if let Some(ref id) = sr.node_id {
                write!(formatter, " | {id}")?;
                if let Some(ref rev) = sr.rev {
                    write!(formatter, " {rev}")?;
                }
            }
            if let Some(usages) = sr.usages {
                write!(formatter, " | usages: {usages}")?;
            }
            if let Some(count) = sr.count {
                write!(formatter, " | count: {count}")?;
            }
            writeln!(formatter)?;
        }
        if self.total > self.results.len() {
            writeln!(
                formatter,
                "({} of {} shown)",
                self.results.len(),
                self.total,
            )?;
        }
        // The master rev of the set this FIND armed — quoted back in IF REV by
        // a bulk LAST mutation. Absent on a truncated result, by design.
        if let Some(ref rev) = self.found_rev {
            writeln!(formatter, "found_rev: {rev}")?;
        }
        Ok(())
    }
}
impl fmt::Display for ShowResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref symbol) = self.symbol {
            writeln!(formatter, "--- {symbol} ---")?;
        }
        if let Some(ref file) = self.file {
            writeln!(formatter, "File: {}", file.display())?;
        }
        if let (Some(start), Some(end)) = (self.start_line, self.end_line) {
            writeln!(formatter, "Lines: {start}\u{2013}{end}")?;
        }
        match &self.content {
            ShowContent::Lines { lines, .. } => {
                for source_line in lines {
                    if let Some(ref marker) = source_line.marker {
                        write!(formatter, "{marker} ")?;
                    }
                    writeln!(formatter, "{:>4} | {}", source_line.line, source_line.text,)?;
                }
            }
            ShowContent::Signature {
                signature, line, ..
            } => {
                writeln!(formatter, "{line:>4} | {signature}")?;
            }
            ShowContent::Outline { entries } => {
                for entry in entries {
                    write_tree_row(
                        formatter,
                        entry.line,
                        &entry.fql_kind,
                        &entry.name,
                        entry.node_id.as_deref(),
                        entry.rev.as_deref(),
                    )?;
                }
            }
            ShowContent::Members { members, .. } => {
                for member in members {
                    write_tree_row(
                        formatter,
                        member.line,
                        &member.fql_kind,
                        &member.text,
                        member.node_id.as_deref(),
                        member.rev.as_deref(),
                    )?;
                }
            }
            ShowContent::CallGraph { entries, .. } => {
                for entry in entries {
                    write!(formatter, "  {}", entry.name)?;
                    if let Some(ref path) = entry.path {
                        write!(formatter, " ({})", path.display())?;
                    }
                    if let Some(line) = entry.line {
                        write!(formatter, ":{line}")?;
                    }
                    writeln!(formatter)?;
                }
            }
            ShowContent::FileList { files, total } => {
                write_file_list(formatter, files, *total, self.metadata.as_ref())?;
            }
            ShowContent::Stats { sessions } =>
            {
                #[allow(clippy::cast_precision_loss)]
                for s in sessions {
                    writeln!(
                        formatter,
                        "session: {} ({}@{})",
                        s.session_id, s.source, s.branch
                    )?;
                    writeln!(
                        formatter,
                        "  rows: {}  names: {}  paths: {}",
                        s.rows, s.distinct_names, s.distinct_paths
                    )?;
                    writeln!(
                        formatter,
                        "  mem_total: {:.1} MB",
                        s.mem_total_bytes as f64 / 1_048_576.0
                    )?;
                }
            }
        }
        if let Some(ref hint) = self.hint {
            writeln!(formatter, "{hint}")?;
        }
        Ok(())
    }
}

impl fmt::Display for MutationResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.applied { "Applied" } else { "Planned" };
        writeln!(
            formatter,
            "{status}: {} edit(s) in {} file(s)",
            self.edit_count,
            self.files_changed.len(),
        )?;
        for path in &self.files_changed {
            writeln!(formatter, "  {}", path.display())?;
        }
        if let Some(ref diff) = self.diff {
            writeln!(formatter, "\n{diff}")?;
        }
        for suggestion in &self.suggestions {
            writeln!(
                formatter,
                "  note: {} ({}:{})",
                suggestion.snippet,
                suggestion.path.display(),
                suggestion.byte_offset,
            )?;
        }
        for se in &self.structural_errors {
            writeln!(
                formatter,
                "  structural error ({}): {}",
                se.path.display(),
                se.message,
            )?;
        }
        Ok(())
    }
}

impl fmt::Display for SourceOpResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(ref name) = self.source_name {
            write!(formatter, "Source: {name}")?;
        }
        if let Some(ref session_id) = self.session_id {
            write!(formatter, "  Session: {session_id}")?;
        }
        if let Some(count) = self.symbols_indexed {
            write!(formatter, "  ({count} symbols indexed)")?;
        }
        if self.resumed {
            write!(formatter, "  (resumed)")?;
        }
        writeln!(formatter)?;
        if let Some(ref message) = self.message {
            writeln!(formatter, "{message}")?;
        }
        if !self.branches.is_empty() {
            writeln!(formatter, "Branches:")?;
            for branch in &self.branches {
                writeln!(formatter, "  {branch}")?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for BeginTransactionResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "Checkpoint '{name}' created (oid: {oid})",
            name = self.name,
            oid = self.checkpoint_oid,
        )
    }
}

impl fmt::Display for CommitResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "Committed: {hash}\nMessage: {msg}",
            hash = self.commit_hash,
            msg = self.message,
        )
    }
}

impl fmt::Display for RollbackResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            formatter,
            "Rolled back to checkpoint '{name}' (oid: {oid})",
            name = self.name,
            oid = self.reset_to_oid,
        )
    }
}

impl fmt::Display for VerifyBuildResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.success { "PASSED" } else { "FAILED" };
        writeln!(formatter, "VERIFY build '{}': {status}", self.step)?;
        if !self.output.is_empty() {
            writeln!(formatter, "{}", self.output.trim_end())?;
        }
        Ok(())
    }
}

impl fmt::Display for RunResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.success { "PASSED" } else { "FAILED" };
        writeln!(formatter, "RUN '{}': {status}", self.step)?;
        if !self.output.is_empty() {
            writeln!(formatter, "{}", self.output.trim_end())?;
        }
        Ok(())
    }
}

impl fmt::Display for PlanResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total_edits: usize = self.file_edits.iter().map(|fe| fe.edit_count).sum();
        writeln!(
            formatter,
            "Plan: {total_edits} edit(s) in {} file(s)",
            self.file_edits.len(),
        )?;
        for file_edit in &self.file_edits {
            writeln!(
                formatter,
                "  {} ({} edits)",
                file_edit.path.display(),
                file_edit.edit_count,
            )?;
        }
        if !self.diff.is_empty() {
            writeln!(formatter, "\n{diff}", diff = self.diff)?;
        }
        Ok(())
    }
}
