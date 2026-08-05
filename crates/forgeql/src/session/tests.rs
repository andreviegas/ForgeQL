//! Unit tests for session persistence and resume: config-directory resolution
//! from `XDG_CONFIG_HOME` or `HOME`, the session file path, loading a file that
//! is missing, empty, invalid, partial or complete, saving and its round-trip,
//! and resume, which no-ops without a session id and clears one gone stale.

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
    use forgeql_lang_c::CLanguage;
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

    let registry = Arc::new(LanguageRegistry::new(vec![
        Arc::new(CLanguage),
        Arc::new(CppLanguage),
    ]));
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
