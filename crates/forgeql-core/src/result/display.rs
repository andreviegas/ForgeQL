//! `fmt::Display` implementations for all `ForgeQL` result types.
use super::{
    BeginTransactionResult, CommitResult, ForgeQLResult, MutationResult, PlanResult, QueryResult,
    RollbackResult, ShowContent, ShowResult, SourceOpResult, VerifyBuildResult,
};
use std::fmt;

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
        Ok(())
    }
}
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
                    writeln!(
                        formatter,
                        "{:>4} | {:12} | {}",
                        entry.line, entry.fql_kind, entry.name,
                    )?;
                }
            }
            ShowContent::Members { members, .. } => {
                for member in members {
                    writeln!(
                        formatter,
                        "{:>4} | {:12} | {}",
                        member.line, member.fql_kind, member.text,
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
                for entry in files {
                    writeln!(formatter, "  {}", entry.path.display())?;
                }
                writeln!(formatter, "({total} files)")?;
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
