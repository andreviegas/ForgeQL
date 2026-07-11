//! One-shot mode runner — execute a single FQL statement and exit.

use crate::cli::CliFormat;
use crate::execute::execute_and_print;
use crate::session::{session_load, session_save, session_try_resume};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::query_logger::QueryLogger;

/// Execute one FQL statement, then exit.
///
/// `session_override` lets the `--session` CLI flag inject a session id
/// without the user re-issuing `USE`.
pub(crate) fn run_one_shot(
    mut engine: ForgeQLEngine,
    fql: &str,
    session_override: Option<&str>,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) {
    let mut session = session_load();
    if let Some(sid) = session_override {
        session.session_id = Some(sid.to_string());
    }
    session_try_resume(&mut engine, &mut session);
    execute_and_print(&mut engine, fql, &mut session, logger.as_mut(), format);
    session_save(&session);
}

/// `forgeql gc` — delete stale columnar cache version directories to reclaim
/// disk space. Thin sugar over the `VACUUM` verb so the pruning logic lives
/// once in the engine: it previews (`VACUUM …`), asks for confirmation unless
/// `yes`, then applies (`VACUUM … APPLY`).
pub(crate) fn run_gc(
    mut engine: ForgeQLEngine,
    source: Option<&str>,
    keep: usize,
    all: bool,
    yes: bool,
    mut logger: Option<QueryLogger>,
    format: CliFormat,
) {
    use std::io::Write as _;

    // Build the clause shared by the preview and apply statements.
    let source_clause = source.map(|s| format!(" SOURCE '{s}'")).unwrap_or_default();
    let keep_clause = if keep > 0 {
        format!(" KEEP {keep}")
    } else {
        String::new()
    };
    let all_clause = if all { " ALL" } else { "" };
    let clause = format!("{source_clause}{keep_clause}{all_clause}");

    let mut session = session_load();

    // Preview: report what would be deleted; delete nothing.
    let preview = format!("VACUUM{clause}");
    execute_and_print(&mut engine, &preview, &mut session, logger.as_mut(), format);

    if !yes {
        eprint!("\nProceed with deletion? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        let confirmed = std::io::stdin().read_line(&mut answer).is_ok()
            && matches!(answer.trim(), "y" | "Y" | "yes" | "YES");
        if !confirmed {
            eprintln!("Aborted. Nothing deleted.");
            return;
        }
    }

    // Apply the deletion.
    let apply = format!("VACUUM{clause} APPLY");
    execute_and_print(&mut engine, &apply, &mut session, logger.as_mut(), format);
}
