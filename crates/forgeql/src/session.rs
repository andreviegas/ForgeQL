//! Session file persistence and session resume logic.
//!
//! A `SessionFile` is a small JSON document written to
//! `~/.config/forgeql/session.json` after every CLI invocation.
//! It allows the next invocation to silently re-connect to the
//! same source/branch without the user needing to re-issue `USE`.

use std::path::PathBuf;

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

    match engine.execute(None, &use_op) {
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::unwrap_in_result)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    // ------------------------------------------------------------------
    // session_config_dir_from  (pure: no env mutation needed)
    // ------------------------------------------------------------------

    fn os(s: &str) -> std::ffi::OsString {
        std::ffi::OsString::from(s)
    }

    #[test]
    fn session_config_dir_from_uses_xdg_when_both_set() {
        let dir = session_config_dir_from(Some(os("/xdg")), Some(os("/home/user"))).unwrap();
        assert_eq!(dir, Path::new("/xdg/forgeql"));
    }

    #[test]
    fn session_config_dir_from_falls_back_to_home_dot_config() {
        let dir = session_config_dir_from(None, Some(os("/home/user"))).unwrap();
        assert_eq!(dir, Path::new("/home/user/.config/forgeql"));
    }

    #[test]
    fn session_config_dir_from_returns_none_when_both_absent() {
        assert!(session_config_dir_from(None, None).is_none());
    }

    #[test]
    fn session_config_dir_from_ignores_home_when_xdg_present() {
        // XDG_CONFIG_HOME must win over HOME when both are set.
        let dir = session_config_dir_from(Some(os("/xdg")), None).unwrap();
        assert_eq!(dir, Path::new("/xdg/forgeql"));
    }

    // ------------------------------------------------------------------
    // session_file_path: derived from session_config_dir, tested via _from
    // ------------------------------------------------------------------

    #[test]
    fn session_file_path_appends_session_json() {
        let base = session_config_dir_from(Some(os("/xdg")), None).unwrap();
        let path = base.join("session.json");
        assert_eq!(path.file_name().unwrap(), "session.json");
        assert_eq!(path, Path::new("/xdg/forgeql/session.json"));
    }

    // ------------------------------------------------------------------
    // session_load_from  (pure path-based, no env)
    // ------------------------------------------------------------------

    #[test]
    fn session_load_from_returns_default_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("no_such_file.json");
        assert_eq!(session_load_from(&path), SessionFile::default());
    }

    #[test]
    fn session_load_from_returns_default_on_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.json");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(session_load_from(&path), SessionFile::default());
    }

    #[test]
    fn session_load_from_returns_default_on_invalid_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.json");
        std::fs::write(&path, b"{ not valid json }").unwrap();
        assert_eq!(session_load_from(&path), SessionFile::default());
    }

    #[test]
    fn session_load_from_deserializes_all_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.json");
        let json = r#"{
            "session_id": "abc123",
            "source": "pisco-code",
            "branch": "main",
            "as_branch": "my-session"
        }"#;
        std::fs::write(&path, json).unwrap();

        let sf = session_load_from(&path);
        assert_eq!(sf.session_id.as_deref(), Some("abc123"));
        assert_eq!(sf.source.as_deref(), Some("pisco-code"));
        assert_eq!(sf.branch.as_deref(), Some("main"));
        assert_eq!(sf.as_branch.as_deref(), Some("my-session"));
    }

    #[test]
    fn session_load_from_optional_fields_default_to_none() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.json");
        std::fs::write(&path, r#"{"session_id": "only-id"}"#).unwrap();

        let sf = session_load_from(&path);
        assert_eq!(sf.session_id.as_deref(), Some("only-id"));
        assert!(sf.source.is_none());
        assert!(sf.branch.is_none());
        assert!(sf.as_branch.is_none());
    }

    // ------------------------------------------------------------------
    // session_save_to  (pure path-based, no env)
    // ------------------------------------------------------------------

    #[test]
    fn session_save_to_creates_parent_dirs_and_writes_json() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("sub").join("dir").join("session.json");
        let sf = SessionFile {
            session_id: Some("sid-99".into()),
            source: Some("my-repo".into()),
            branch: Some("dev".into()),
            as_branch: Some("agent-session".into()),
        };
        session_save_to(&sf, &path);

        assert!(path.exists(), "session.json should be created");
        let written: SessionFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written, sf);
    }

    #[test]
    fn session_save_to_overwrites_previous_content() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.json");

        session_save_to(
            &SessionFile {
                session_id: Some("first".into()),
                ..Default::default()
            },
            &path,
        );
        session_save_to(
            &SessionFile {
                session_id: Some("second".into()),
                ..Default::default()
            },
            &path,
        );

        let loaded = session_load_from(&path);
        assert_eq!(loaded.session_id.as_deref(), Some("second"));
    }

    #[test]
    fn session_save_to_roundtrip_preserves_all_fields() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.json");
        let original = SessionFile {
            session_id: Some("rt-id".into()),
            source: Some("src".into()),
            branch: Some("br".into()),
            as_branch: Some("ab".into()),
        };
        session_save_to(&original, &path);
        assert_eq!(session_load_from(&path), original);
    }

    // ------------------------------------------------------------------
    // session_try_resume
    // ------------------------------------------------------------------

    fn make_test_engine() -> (ForgeQLEngine, TempDir) {
        use forgeql_core::ast::lang::LanguageRegistry;
        use forgeql_lang_cpp::CppLanguage;
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("tests/fixtures");
        let _ = std::fs::copy(
            fixtures.join("motor_control.h"),
            tmp.path().join("motor_control.h"),
        )
        .expect("copy motor_control.h");
        let _ = std::fs::copy(
            fixtures.join("motor_control.cpp"),
            tmp.path().join("motor_control.cpp"),
        )
        .expect("copy motor_control.cpp");

        let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
        let data_dir = tmp.path().join("data");
        let engine = ForgeQLEngine::new(data_dir, registry).unwrap();
        (engine, tmp)
    }

    #[test]
    fn session_try_resume_noop_when_session_id_missing() {
        let (mut engine, _tmp) = make_test_engine();
        let mut sf = SessionFile::default();
        session_try_resume(&mut engine, &mut sf);
        assert!(sf.session_id.is_none());
    }

    #[test]
    fn session_try_resume_clears_id_when_source_missing() {
        let (mut engine, tmp) = make_test_engine();
        let mut sf = SessionFile {
            session_id: Some("stale".into()),
            source: None,
            branch: Some("main".into()),
            as_branch: Some("s".into()),
        };
        let _ = engine.register_local_session(tmp.path());
        session_try_resume(&mut engine, &mut sf);
        assert!(sf.session_id.is_none());
    }

    #[test]
    fn session_try_resume_clears_id_when_branch_missing() {
        let (mut engine, tmp) = make_test_engine();
        let mut sf = SessionFile {
            session_id: Some("stale".into()),
            source: Some("local".into()),
            branch: None,
            as_branch: Some("s".into()),
        };
        let _ = engine.register_local_session(tmp.path());
        session_try_resume(&mut engine, &mut sf);
        assert!(sf.session_id.is_none());
    }

    #[test]
    fn session_try_resume_clears_id_when_as_branch_missing() {
        let (mut engine, tmp) = make_test_engine();
        let mut sf = SessionFile {
            session_id: Some("stale".into()),
            source: Some("local".into()),
            branch: Some("main".into()),
            as_branch: None,
        };
        let _ = engine.register_local_session(tmp.path());
        session_try_resume(&mut engine, &mut sf);
        assert!(sf.session_id.is_none());
    }

    #[test]
    fn session_try_resume_resets_session_on_engine_error() {
        let (mut engine, _tmp) = make_test_engine();
        let mut sf = SessionFile {
            session_id: Some("stale".into()),
            source: Some("nonexistent-source".into()),
            branch: Some("main".into()),
            as_branch: Some("my-session".into()),
        };
        // No registered source → engine.execute will fail → full reset.
        session_try_resume(&mut engine, &mut sf);
        assert_eq!(sf, SessionFile::default());
    }
}
