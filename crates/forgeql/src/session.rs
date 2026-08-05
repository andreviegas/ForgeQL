//! Session file persistence and session resume logic.
//!
//! A `SessionFile` is a small JSON document written to
//! `~/.config/forgeql/session.json` after every CLI invocation.
//! It allows the next invocation to silently re-connect to the
//! same source/branch without the user needing to re-issue `USE`.

use std::path::PathBuf;

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::ir::ForgeQLIR;
use forgeql_core::result::{ForgeQLResult, SourceOpResult};
use serde::{Deserialize, Serialize};
use tracing::info;

// -----------------------------------------------------------------------
// Data model
// -----------------------------------------------------------------------

/// Persistent per-user session state written to disk between invocations.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct SessionFile {
    /// The in-memory session id from the last `USE` command.
    pub(crate) session_id: Option<String>,
    /// Source name for auto-resume (e.g. `"pisco-code"`).
    #[serde(default)]
    pub(crate) source: Option<String>,
    /// Branch name for auto-resume (e.g. `"main"`).
    #[serde(default)]
    pub(crate) branch: Option<String>,
    /// Custom branch alias from `USE … AS 'name'`.
    #[serde(default)]
    pub(crate) as_branch: Option<String>,
}

// -----------------------------------------------------------------------
// File-system helpers
// -----------------------------------------------------------------------
///
/// Respects `XDG_CONFIG_HOME`; falls back to `$HOME/.config`.
/// Returns `None` when neither env var resolves.
pub(crate) fn session_config_dir() -> Option<PathBuf> {
    session_config_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Pure inner logic for [`session_config_dir`], injectable for testing.
fn session_config_dir_from(
    xdg_config_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    let config_dir = xdg_config_home
        .map(PathBuf::from)
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(config_dir.join("forgeql"))
}

/// Return the full path to the session JSON file.
pub(crate) fn session_file_path() -> Option<PathBuf> {
    session_config_dir().map(|d| d.join("session.json"))
}

/// Load the session file from disk.
///
/// Returns [`SessionFile::default`] when the file does not exist,
/// cannot be read, or contains invalid JSON.
pub(crate) fn session_load() -> SessionFile {
    session_file_path().map_or_else(SessionFile::default, |path| session_load_from(&path))
}

/// Persist the session file to disk.
///
/// Creates parent directories as needed.  Errors are silently ignored
/// (a failed write is non-fatal — the user just won't have auto-resume).
pub(crate) fn session_save(sf: &SessionFile) {
    if let Some(path) = session_file_path() {
        session_save_to(sf, &path);
    }
}

/// Inner load logic: reads and deserialises `path`, returning `Default` on any error.
fn session_load_from(path: &std::path::Path) -> SessionFile {
    let Ok(data) = std::fs::read_to_string(path) else {
        return SessionFile::default();
    };
    serde_json::from_str(&data).unwrap_or_default()
}

/// Inner save logic: creates parent dirs then writes pretty JSON to `path`.
fn session_save_to(sf: &SessionFile, path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(sf) {
        let _ = std::fs::write(path, json);
    }
}

// -----------------------------------------------------------------------
// Resume logic
// -----------------------------------------------------------------------

/// Attempt to re-connect to a saved session across CLI invocations.
///
/// Each CLI process starts with a fresh `ForgeQLEngine` (no in-memory
/// sessions).  When the session file records a previous `session_id`
/// plus the `source/branch/as_branch` that created it, this function
/// silently re-executes `USE source.branch AS 'as_branch'` to restore
/// the session.
///
/// | Outcome                       | `session` mutation         |
/// |-------------------------------|----------------------------|
/// | Successful resume             | `session_id` → new id      |
/// | `session_id` absent           | no-op (nothing to resume)  |
/// | `source`/`branch` absent      | `session_id` cleared       |
/// | `as_branch` absent            | `session_id` cleared       |
/// | Engine rejects the USE        | `session` fully reset      |
pub(crate) fn session_try_resume(engine: &mut ForgeQLEngine, session: &mut SessionFile) {
    let Some(ref old_sid) = session.session_id else {
        return;
    };
    let (Some(source), Some(branch)) = (&session.source, &session.branch) else {
        // No source/branch info — legacy session file; clear the stale id.
        info!("session file has no source/branch info — clearing stale session");
        session.session_id = None;
        return;
    };
    let Some(ref as_branch) = session.as_branch else {
        // No AS branch — legacy session without AS clause; clear the stale id.
        info!("session file has no as_branch — clearing stale session");
        session.session_id = None;
        return;
    };

    let use_op = ForgeQLIR::UseSource {
        source: source.clone(),
        branch: branch.clone(),
        as_branch: as_branch.clone(),
    };

    // user_id is the single birth point for identity in session-restore mode.
    // When auth is implemented, replace this with a real credential lookup.
    let user_id = auth(AuthContext::Session);
    match engine.execute(user_id, None, &use_op).result {
        Ok(ForgeQLResult::SourceOp(SourceOpResult {
            session_id: Some(ref new_sid),
            ..
        })) => {
            info!(%old_sid, %new_sid, %source, %branch, "session resumed");
            session.session_id = Some(new_sid.clone());
        }
        Ok(_) => {
            info!("USE did not return a session — clearing stale session");
            session.session_id = None;
        }
        Err(err) => {
            info!(%err, "failed to resume session — clearing stale session");
            *session = SessionFile::default();
        }
    }
}
// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test code")]
mod tests;
