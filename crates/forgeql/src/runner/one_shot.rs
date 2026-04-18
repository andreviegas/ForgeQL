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
