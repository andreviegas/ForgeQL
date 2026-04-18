//! Pipe mode runner — reads FQL statements from stdin line-by-line.

use anyhow::{Context, Result};
use std::io::BufRead;

use crate::cli::CliFormat;
use crate::execute::execute_and_print;
use crate::session::{session_load, session_save, session_try_resume};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::query_logger::QueryLogger;

/// Run in pipe mode: read FQL lines from stdin, execute each one.
///
/// Blank lines and lines starting with `#` are skipped (comment syntax).
/// Session state is loaded at startup and saved after the last line.
pub(crate) fn run_pipe(
    mut engine: ForgeQLEngine,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) -> Result<()> {
    let mut session = session_load();
    session_try_resume(&mut engine, &mut session);

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        execute_and_print(&mut engine, trimmed, &mut session, logger.as_mut(), format);
    }

    session_save(&session);
    Ok(())
}
