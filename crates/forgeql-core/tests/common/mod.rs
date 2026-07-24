//! Shared test harness for the `forgeql-core` integration suites.
//!
//! Cargo treats `tests/common/mod.rs` as a shared module (not its own test
//! binary), pulled into a suite with `mod common;`. It owns the one language
//! registry, the session setup/teardown mechanism, and the exec/assert helpers
//! that every suite used to copy-paste.
//!
//! Rust has no `@Before`/`@After`; the equivalents live here:
//!  - `setup()`  → [`legacy_session`] / [`columnar_session`] build a fresh temp
//!    workspace, copy fixtures, and register a session.
//!  - `teardown()` → [`TestSession`]'s `Drop` frees the temp workspace at end of
//!    scope, even on panic — strictly better than a teardown a failing test skips.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    unreachable_pub
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::ForgeQLResult;
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

/// The single language registry every suite shares — the one place the language
/// set is defined: the production `forgeql-lang-*` plugins (`CppLanguage`,
/// `RustLanguage`, `PythonLanguage`) plus `text_languages()`.
pub fn make_registry() -> Arc<LanguageRegistry> {
    let mut langs = forgeql_lang_text::text_languages();
    langs.push(Arc::new(forgeql_lang_cpp::CppLanguage));
    langs.push(Arc::new(forgeql_lang_rust::RustLanguage));
    langs.push(Arc::new(forgeql_lang_python::PythonLanguage));
    Arc::new(LanguageRegistry::new(langs))
}

/// The repository `tests/fixtures` directory. Fixture arguments to the session
/// constructors are paths relative to here (e.g. `"canonical/canonical.cpp"`);
/// each is copied into the temp workspace under its file name.
pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/fixtures")
}

/// The handle an agent would read off a `FIND files` row, computed the same way
/// the engine does: `n` + 12 hex of the SHA-256 of the workspace-relative path.
pub fn path_handle(rel: &str) -> String {
    format!(
        "n{}",
        forgeql_core::node_id::hex_prefix(&forgeql_core::node_id::sha256_of_path(rel), 12)
    )
}

/// Copy each fixture (a path under `tests/fixtures`) into `dir`, flattening to
/// its file name.
pub fn copy_fixtures(dir: &Path, fixtures: &[&str]) {
    let src = fixtures_dir();
    for rel in fixtures {
        let name = Path::new(rel).file_name().expect("fixture has a file name");
        let _ = fs::copy(src.join(rel), dir.join(name)).expect("copy fixture");
    }
}

/// RAII test session — the `setup()`/`teardown()` mechanism. Owns the engine, its
/// session id, and the temp workspace; `Drop` (fields drop in declaration order:
/// engine first, temp dir last) frees the workspace at end of scope, even on panic.
pub struct TestSession {
    pub engine: ForgeQLEngine,
    pub sid: String,
    dir: tempfile::TempDir,
}

impl TestSession {
    /// Parse FQL and execute the first op; panics on error (the common path).
    pub fn exec(&mut self, fql: &str) -> ForgeQLResult {
        let ops = parser::parse(fql).expect("parse");
        let op = ops.first().expect("at least one op");
        let coords = SessionCoords::from_session_id(&self.sid).expect("valid session_id");
        self.engine
            .execute(auth(AuthContext::Tester), Some(&coords), op)
            .result
            .expect("execute")
    }

    /// Parse and execute, returning the error instead of panicking on it.
    pub fn try_fql(&mut self, fql: &str) -> anyhow::Result<ForgeQLResult> {
        let ops = parser::parse(fql).expect("parse");
        let op = ops.first().expect("at least one op");
        let coords = SessionCoords::from_session_id(&self.sid).expect("valid session_id");
        self.engine
            .execute(auth(AuthContext::Tester), Some(&coords), op)
            .result
    }

    /// Like [`Self::exec`] but waits for any background job the op spawns
    /// (JOB / VERIFY) to finish — the deterministic choice for gate/job tests.
    pub fn exec_blocking(&mut self, fql: &str) -> ForgeQLResult {
        let ops = parser::parse(fql).expect("parse");
        let op = ops.first().expect("at least one op");
        let coords = SessionCoords::from_session_id(&self.sid).expect("valid session_id");
        self.engine
            .execute_blocking(auth(AuthContext::Tester), Some(&coords), op)
            .result
            .expect("execute")
    }

    /// Blocking variant of [`Self::try_fql`] — returns the error instead of panicking.
    pub fn try_fql_blocking(&mut self, fql: &str) -> anyhow::Result<ForgeQLResult> {
        let ops = parser::parse(fql).expect("parse");
        let op = ops.first().expect("at least one op");
        let coords = SessionCoords::from_session_id(&self.sid).expect("valid session_id");
        self.engine
            .execute_blocking(auth(AuthContext::Tester), Some(&coords), op)
            .result
    }

    /// Run a statement that must be refused, and hand back the refusal message —
    /// the message is the contract for every gate.
    pub fn err(&mut self, fql: &str) -> String {
        self.try_fql(fql).expect_err("must be refused").to_string()
    }

    /// `(node_id, rev)` for a workspace file: the handle computed like the engine
    /// does, plus its current rev read back via `FIND NODE`.
    pub fn file_handle(&mut self, rel: &str) -> (String, String) {
        let handle = path_handle(rel);
        let rev = self.node_rev(&handle);
        (handle, rev)
    }

    /// Current rev of a node handle, via `FIND NODE`.
    pub fn node_rev(&mut self, handle: &str) -> String {
        match self.exec(&format!("FIND NODE '{handle}'")) {
            ForgeQLResult::FindNode(node) => node.rev,
            other => panic!("expected FindNode, got {other:?}"),
        }
    }

    /// The temp workspace root the session was registered on — needed by tests
    /// that inspect on-disk artifacts (e.g. `.forgeql-checkpoints`).
    pub fn workspace(&self) -> &std::path::Path {
        self.dir.path()
    }

    /// Consume the session into its owned `(engine, session_id, TempDir)` parts.
    /// For suites mid-migration that still thread the three pieces explicitly
    /// rather than calling the methods above; the `TempDir` must stay alive.
    pub fn into_parts(self) -> (ForgeQLEngine, String, tempfile::TempDir) {
        (self.engine, self.sid, self.dir)
    }
}

/// `setup()` on the legacy in-memory backend: fresh temp workspace, `fixtures`
/// copied in, a local session registered.
pub fn legacy_session(fixtures: &[&str]) -> TestSession {
    let dir = tempdir().expect("tempdir");
    copy_fixtures(dir.path(), fixtures);
    legacy_session_in(dir)
}

/// Register a legacy-backend session over an already-populated temp workspace.
/// Use when a test writes bespoke files rather than copying named fixtures.
pub fn legacy_session_in(dir: tempfile::TempDir) -> TestSession {
    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    let sid = engine
        .register_local_session(dir.path())
        .expect("register session");
    TestSession { engine, sid, dir }
}

/// `setup()` on the columnar backend — the production read path. Mirrors a real
/// `USE`: builds `segments`/`overlays` dirs under the temp workspace and installs
/// columnar via `register_local_session_with_columnar`.
pub fn columnar_session(fixtures: &[&str]) -> TestSession {
    let dir = tempdir().expect("tempdir");
    copy_fixtures(dir.path(), fixtures);
    columnar_session_in(dir)
}

/// Register a columnar-backend session over an already-populated temp workspace.
pub fn columnar_session_in(dir: tempfile::TempDir) -> TestSession {
    let data_dir = dir.path().join("data");
    let mut engine = ForgeQLEngine::new(data_dir, make_registry()).expect("engine");
    let segments_dir = dir.path().join("segments");
    let overlays_dir = dir.path().join("overlays");
    let sid = engine
        .register_local_session_with_columnar(dir.path(), &segments_dir, &overlays_dir)
        .expect("register columnar session");
    TestSession { engine, sid, dir }
}

// -----------------------------------------------------------------------
// Tuple-style adapters — for suites that thread the explicit
// `(engine, session_id, TempDir)` parts rather than calling the TestSession
// methods above. The `TempDir` must stay alive.
// -----------------------------------------------------------------------

/// Boot a columnar session over `motor_control` fixtures plus any extras.
/// Returns `(engine, session_id, TempDir)`; the `TempDir` must stay alive.
pub fn engine_with_session_with_extra_files(
    extra_files: &[&str],
) -> (ForgeQLEngine, String, tempfile::TempDir) {
    let mut fixtures = vec!["motor_control.h", "motor_control.cpp"];
    fixtures.extend_from_slice(extra_files);
    columnar_session(&fixtures).into_parts()
}

/// Columnar session over just the `motor_control` fixtures.
pub fn engine_with_session() -> (ForgeQLEngine, String, tempfile::TempDir) {
    engine_with_session_with_extra_files(&[])
}

/// Legacy-backend variant of [`engine_with_session`]. Pins tests that document
/// known legacy/columnar behaviour divergences.
pub fn engine_with_session_legacy() -> (ForgeQLEngine, String, tempfile::TempDir) {
    legacy_session(&["motor_control.h", "motor_control.cpp"]).into_parts()
}

/// Parse FQL and execute the first op against the engine.
pub fn execute_fql(engine: &mut ForgeQLEngine, session_id: &str, fql: &str) -> ForgeQLResult {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    let coords = SessionCoords::from_session_id(session_id).expect("valid session_id");
    engine
        .execute(auth(AuthContext::Tester), Some(&coords), op)
        .result
        .expect("execute")
}

/// Parse and execute, returning the error instead of panicking on it.
pub fn try_fql(
    engine: &mut ForgeQLEngine,
    session_id: &str,
    fql: &str,
) -> anyhow::Result<ForgeQLResult> {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("at least one op");
    let coords = SessionCoords::from_session_id(session_id).expect("valid session_id");
    engine
        .execute(auth(AuthContext::Tester), Some(&coords), op)
        .result
}

/// Run a statement that must be refused, and hand back the refusal message —
/// the message is the contract for every gate.
pub fn fql_err(engine: &mut ForgeQLEngine, session_id: &str, fql: &str) -> String {
    try_fql(engine, session_id, fql)
        .expect_err("must be refused")
        .to_string()
}

/// Current rev of a node handle, via `FIND NODE`.
pub fn node_rev(engine: &mut ForgeQLEngine, session_id: &str, handle: &str) -> String {
    match execute_fql(engine, session_id, &format!("FIND NODE '{handle}'")) {
        ForgeQLResult::FindNode(node) => node.rev,
        other => panic!("expected FindNode, got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Result extractors — pull a typed result out of a `ForgeQLResult` or panic.
// -----------------------------------------------------------------------

/// Extract the query result or panic.
pub fn as_query(r: &ForgeQLResult) -> &forgeql_core::result::QueryResult {
    match r {
        ForgeQLResult::Query(qr) => qr,
        other => panic!("expected Query, got: {other:?}"),
    }
}

/// Extract the show result or panic.
pub fn as_show(r: &ForgeQLResult) -> &forgeql_core::result::ShowResult {
    match r {
        ForgeQLResult::Show(sr) => sr,
        other => panic!("expected Show, got: {other:?}"),
    }
}

/// Extract the mutation result or panic.
pub fn as_mutation(r: &ForgeQLResult) -> &forgeql_core::result::MutationResult {
    match r {
        ForgeQLResult::Mutation(mr) => mr,
        other => panic!("expected Mutation, got: {other:?}"),
    }
}
