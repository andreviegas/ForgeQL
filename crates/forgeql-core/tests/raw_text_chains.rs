//! Raw-text `CHANGE FILE` / `COPY LINES` / `MOVE LINES` chains, driven through
//! the engine on a throwaway tempdir corpus.
//!
//! `CHANGE FILE` refuses indexed source files unless
//! `FORGEQL_ALLOW_CHANGE_FILE_INDEXED` is set. The golden suites pin the refusal,
//! because that is the contract an agent meets; this suite owns the other side —
//! what the raw-text verbs actually do once the override lets them through. It
//! sets the variable itself instead of inheriting it from whatever launched the
//! run, so the coverage does not depend on how the suite was invoked.
//!
//! File contents are compared line by line: this suite pins what the verbs write,
//! not how a trailing newline is normalised.
//!
//! Run with: `cargo test -p forgeql-core --test raw_text_chains`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    unused_results
)]

use std::fs;
use std::path::{Path, PathBuf};

use forgeql_core::auth::{AuthContext, auth};
use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::parser;
use forgeql_core::result::{ForgeQLResult, MutationResult};
use forgeql_core::session::SessionCoords;
use tempfile::tempdir;

mod common;

/// The environment variable that re-enables raw-text editing of indexed files.
const FLAG: &str = "FORGEQL_ALLOW_CHANGE_FILE_INDEXED";

const ALPHA: &str = "\
void alpha_one(void) { shared_api(); }
void alpha_two(void) { shared_api(); }
void alpha_three(void) { shared_api(); }
void alpha_four(void) { shared_api(); }
void alpha_five(void) { shared_api(); }
";

const BETA: &str = "\
void beta_one(void) { shared_api(); }
void beta_two(void) { shared_api(); }
void beta_three(void) { shared_api(); }
";

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

/// Set or clear the override. The engine re-reads it on every `CHANGE FILE`.
///
/// `set_var` / `remove_var` are unsafe in edition 2024 because another thread
/// reading the environment concurrently would be a data race. Both call sites
/// below run before their engine exists, on a single-threaded test binary.
#[expect(
    unsafe_code,
    reason = "single-threaded test binary; each call precedes engine construction"
)]
fn set_override(on: bool) {
    if on {
        unsafe { std::env::set_var(FLAG, "1") };
    } else {
        unsafe { std::env::remove_var(FLAG) };
    }
}

/// Execute one statement, surfacing the engine's error text rather than panicking.
fn try_exec(
    engine: &mut ForgeQLEngine,
    session_id: Option<&str>,
    fql: &str,
) -> Result<ForgeQLResult, String> {
    let ops = parser::parse(fql).expect("parse");
    let op = ops.first().expect("one op");
    let coords = session_id.map(|s| SessionCoords::from_session_id(s).expect("valid session_id"));
    engine
        .execute(auth(AuthContext::Tester), coords.as_ref(), op)
        .result
        .map_err(|e| e.to_string())
}

/// Execute one statement that must succeed.
fn exec(engine: &mut ForgeQLEngine, session_id: Option<&str>, fql: &str) -> ForgeQLResult {
    try_exec(engine, session_id, fql).unwrap_or_else(|e| panic!("{fql}: {e}"))
}

/// Execute one mutation and return its result.
fn mutate(engine: &mut ForgeQLEngine, session_id: Option<&str>, fql: &str) -> MutationResult {
    match exec(engine, session_id, fql) {
        ForgeQLResult::Mutation(m) => m,
        _ => panic!("{fql}: expected a mutation result"),
    }
}

/// A worktree file's contents, split into lines.
fn lines(wt: &Path, rel: &str) -> Vec<String> {
    fs::read_to_string(wt.join(rel))
        .unwrap_or_else(|e| panic!("read {rel}: {e}"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Create a non-bare git repo holding the two indexed fixtures, and return the
/// branch its HEAD points at.
fn make_source_repo(dir: &Path) -> String {
    let repo = git2::Repository::init(dir).expect("git init");
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    drop(cfg);

    fs::write(dir.join("alpha.cpp"), ALPHA).expect("write alpha.cpp");
    fs::write(dir.join("beta.cpp"), BETA).expect("write beta.cpp");

    let mut index = repo.index().unwrap();
    index.add_path(Path::new("alpha.cpp")).unwrap();
    index.add_path(Path::new("beta.cpp")).unwrap();
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    {
        // Scope `tree` so the borrow on `repo` is released before HEAD is read.
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(0, 0)).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    repo.head()
        .unwrap()
        .shorthand()
        .unwrap_or("master")
        .to_string()
}

/// Boot an engine over a fresh corpus and open a session on it. Returns the
/// engine, its session token, the worktree path, and the two `TempDir` guards
/// that must outlive the test.
fn engine_with_corpus(
    alias: &str,
) -> (
    ForgeQLEngine,
    String,
    PathBuf,
    tempfile::TempDir,
    tempfile::TempDir,
) {
    let src_dir = tempdir().expect("src tempdir");
    let branch = make_source_repo(src_dir.path());

    let data_dir = tempdir().expect("data tempdir");
    let mut engine =
        ForgeQLEngine::new(data_dir.path().to_path_buf(), common::make_registry()).expect("engine");

    let create = format!("CREATE SOURCE 'raw' FROM '{}'", src_dir.path().display());
    exec(&mut engine, None, &create);
    exec(&mut engine, None, &format!("USE raw.{branch} AS '{alias}'"));

    let wt_path = SessionCoords::user_worktrees_root(data_dir.path(), auth(AuthContext::Tester))
        .join(format!("raw.{branch}.{alias}"));
    assert!(
        wt_path.is_dir(),
        "worktree must exist after USE: {}",
        wt_path.display()
    );

    let session =
        SessionCoords::new(auth(AuthContext::Tester), "raw", &branch, alias).to_session_id();

    (engine, session, wt_path, data_dir, src_dir)
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

/// The whole raw-text surface in one sequential test: the refusal that stands
/// without the override, then every verb the override unlocks.
///
/// One `#[test]`, not two, because the override is process-wide environment
/// state — libtest would otherwise run the phases concurrently and let each flip
/// the flag out from under the other.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "deliberately one sequential test: the phases share process-wide env state"
)]
fn raw_text_chains_under_the_indexed_override() {
    // -- Control. Without the override, CHANGE FILE refuses an indexed file and
    //    writes nothing. This is what gives the rest of the test its meaning: it
    //    proves the edits below succeed *because* of the override. It runs on its
    //    own engine, dropped before the flag is flipped.
    set_override(false);
    {
        let (mut engine, session, wt, _data, _src) = engine_with_corpus("refused");
        let sid = Some(session.as_str());

        let err = try_exec(
            &mut engine,
            sid,
            "CHANGE FILE 'alpha.cpp' MATCHING 'shared_api' WITH 'renamed_api'",
        )
        .expect_err("CHANGE FILE must be refused on an indexed file");

        assert!(
            err.contains("CHANGE FILE is disabled for indexed files"),
            "unexpected refusal text: {err}"
        );
        assert_eq!(
            lines(&wt, "alpha.cpp"),
            ALPHA.lines().collect::<Vec<_>>(),
            "a refused edit must leave the file untouched"
        );
    }

    // -- The allowed path. A fresh corpus on a second engine, with the override
    //    on: the raw-text verbs chained across one worktree.
    set_override(true);
    let (mut engine, session, wt, _data, _src) = engine_with_corpus("chains");
    let sid = Some(session.as_str());

    // One plan spanning two files: eight replacements, both files rewritten.
    let m = mutate(
        &mut engine,
        sid,
        "CHANGE FILES 'alpha.cpp', 'beta.cpp' MATCHING 'shared_api' WITH 'swept_api'",
    );
    assert!(m.applied);
    assert_eq!(m.edit_count, 8);
    assert_eq!(m.files_changed.len(), 2);
    assert!(
        !lines(&wt, "alpha.cpp")
            .iter()
            .any(|l| l.contains("shared_api"))
    );
    assert!(
        !lines(&wt, "beta.cpp")
            .iter()
            .any(|l| l.contains("shared_api"))
    );

    // A line-range splice collapses two lines into one; neighbours are untouched.
    let m = mutate(
        &mut engine,
        sid,
        "CHANGE FILE 'alpha.cpp' LINES 2-3 WITH 'void alpha_merged(void) { swept_api(); }'",
    );
    assert_eq!((m.lines_written, m.lines_removed), (1, 2));
    assert_eq!(
        lines(&wt, "alpha.cpp"),
        [
            "void alpha_one(void) { swept_api(); }",
            "void alpha_merged(void) { swept_api(); }",
            "void alpha_four(void) { swept_api(); }",
            "void alpha_five(void) { swept_api(); }",
        ]
    );

    // COPY writes the destination and leaves the source alone.
    let before_copy = lines(&wt, "alpha.cpp");
    mutate(
        &mut engine,
        sid,
        "COPY LINES 1-2 OF 'alpha.cpp' TO 'gamma.cpp'",
    );
    assert_eq!(
        lines(&wt, "alpha.cpp"),
        before_copy,
        "COPY must not alter the source"
    );
    assert_eq!(
        lines(&wt, "gamma.cpp"),
        [
            "void alpha_one(void) { swept_api(); }",
            "void alpha_merged(void) { swept_api(); }",
        ]
    );

    // MOVE removes from the source as it writes the destination.
    mutate(
        &mut engine,
        sid,
        "MOVE LINES 1-1 OF 'beta.cpp' TO 'delta.cpp'",
    );
    assert_eq!(
        lines(&wt, "beta.cpp"),
        [
            "void beta_two(void) { swept_api(); }",
            "void beta_three(void) { swept_api(); }",
        ]
    );
    assert_eq!(
        lines(&wt, "delta.cpp"),
        ["void beta_one(void) { swept_api(); }"]
    );

    // Nested transactions: the inner rollback restores the outer's edit, not the
    // pre-transaction file; only the outer rollback goes all the way back.
    let before_txn = lines(&wt, "beta.cpp");

    exec(&mut engine, sid, "BEGIN TRANSACTION 'raw-outer'");
    mutate(
        &mut engine,
        sid,
        "CHANGE FILE 'beta.cpp' WITH 'void beta_replaced(void) {}'",
    );
    assert_eq!(lines(&wt, "beta.cpp"), ["void beta_replaced(void) {}"]);

    exec(&mut engine, sid, "BEGIN TRANSACTION 'raw-inner'");
    mutate(
        &mut engine,
        sid,
        "CHANGE FILE 'beta.cpp' MATCHING 'beta_replaced' WITH 'beta_inner'",
    );
    assert_eq!(lines(&wt, "beta.cpp"), ["void beta_inner(void) {}"]);

    exec(&mut engine, sid, "ROLLBACK TRANSACTION 'raw-inner'");
    assert_eq!(
        lines(&wt, "beta.cpp"),
        ["void beta_replaced(void) {}"],
        "the inner rollback must keep the outer transaction's edit"
    );

    exec(&mut engine, sid, "ROLLBACK TRANSACTION 'raw-outer'");
    assert_eq!(lines(&wt, "beta.cpp"), before_txn);

    // A path the transaction brought into existence does not survive its
    // rollback — `git reset --hard` alone would walk straight past it.
    exec(&mut engine, sid, "BEGIN TRANSACTION 'raw-create'");
    mutate(
        &mut engine,
        sid,
        "CHANGE FILE 'nested/created.cpp' WITH 'void created(void) {}'",
    );
    assert!(wt.join("nested/created.cpp").is_file());

    exec(&mut engine, sid, "ROLLBACK TRANSACTION 'raw-create'");
    assert!(
        !wt.join("nested/created.cpp").exists(),
        "ROLLBACK must remove paths the transaction created"
    );
}
