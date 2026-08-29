//! A config file that EXISTS and will not parse must refuse the session, not
//! degrade it.
//!
//! Absence is the designed fallback and stays one: a source with no
//! `.forgeql.yaml` is answered by the in-memory backend, with no columnar index
//! and no VERIFY steps. A file that exists and cannot be read is a different
//! fact, and `load_verify_config` used to report it as the same one — both arms
//! mapped through `.ok()`. Every caller gates real capability on that result, so
//! one mistyped value made a configured source look unconfigured: `USE` answered
//! success with `symbols_indexed 0`, rows carried no `node_id` and no `rev`, and
//! every VERIFY and RUN step reported "add it under `run_steps`:" for a step the
//! file plainly declared.
//!
//! Run with: `cargo test -p forgeql-core --test config_refusal`
//!
//! BOUNDARY: these cases drive `register_local_session`, which is one of the
//! four callers of `load_verify_config`. The other three — `USE` in
//! `exec_source/attach.rs` and the two warm hooks in `exec_source/admin.rs` —
//! are covered by the TYPE rather than by a case here: the function returns
//! `Result<Option<_>>`, so a caller cannot reach a degraded session without
//! writing the error away, and the compiler refuses the old shape. What is
//! asserted here is that the refusal happens at all, that it names the file and
//! the parse failure, and that it reaches a caller's return value.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    unused_results
)]

use std::fs;

use forgeql_core::engine::ForgeQLEngine;
use forgeql_core::result::ForgeQLResult;
use tempfile::tempdir;

mod common;

const GOOD_CPP: &str = "int velocidad(int carga)\n{\n    return carga * 2;\n}\n";

/// Valid YAML, but `verify_steps` is a mapping where a sequence is required —
/// the shape of a real typo rather than a file of noise.
const MALFORMED_YAML: &str = "verify_steps:\n  name: gate\n  command: \"true\"\n";

/// A workspace inside an outer directory, so the SIDECAR path
/// (`<parent>/<source>.forgeql.yaml`) lands somewhere this test owns rather than
/// in the system temp root.
fn workspace_with(outer: &std::path::Path) -> std::path::PathBuf {
    let ws = outer.join("ws");
    fs::create_dir_all(&ws).expect("mkdir ws");
    fs::write(ws.join("power.cpp"), GOOD_CPP).expect("write cpp");
    ws
}

fn engine_for(outer: &std::path::Path) -> ForgeQLEngine {
    ForgeQLEngine::new(outer.join("data"), common::make_registry()).expect("engine")
}

#[test]
fn a_malformed_in_repo_config_refuses_the_session_instead_of_degrading_it() {
    let outer = tempdir().expect("tempdir");
    let ws = workspace_with(outer.path());
    fs::write(ws.join(".forgeql.yaml"), MALFORMED_YAML).expect("write yaml");

    let err = engine_for(outer.path())
        .register_local_session(&ws)
        .expect_err("a config file that will not parse must refuse the session");

    let text = format!("{err:#}");
    assert!(
        text.contains(".forgeql.yaml"),
        "the refusal must name the file that could not be read: {text}"
    );
    assert!(
        text.contains("verify_steps"),
        "the refusal must carry the parse failure, not just a path: {text}"
    );
}

#[test]
fn a_malformed_sidecar_refuses_the_session_instead_of_degrading_it() {
    let outer = tempdir().expect("tempdir");
    let ws = workspace_with(outer.path());
    // The sidecar the loader prefers over any in-repo file:
    // `<parent of repo>/<source_name>.forgeql.yaml`, and a local session's
    // source name is `local`.
    fs::write(outer.path().join("local.forgeql.yaml"), MALFORMED_YAML).expect("write sidecar");

    let err = engine_for(outer.path())
        .register_local_session(&ws)
        .expect_err("a sidecar that will not parse must refuse the session");

    let text = format!("{err:#}");
    assert!(
        text.contains("local.forgeql.yaml"),
        "the refusal must name the SIDECAR, which is the file actually read: {text}"
    );
    assert!(
        text.contains("verify_steps"),
        "the refusal must carry the parse failure, not just a path: {text}"
    );
}

/// The control, and the behaviour this change must NOT alter: no config file at
/// all is not an error, and the session it produces still answers.
///
/// Without this, refusing an unreadable file and refusing an absent one would
/// look identical from the outside, and the fallback the changelog promises
/// would be gone with nothing to report it.
#[test]
fn control_no_config_file_still_answers_from_the_in_memory_backend() {
    let outer = tempdir().expect("tempdir");
    let ws = workspace_with(outer.path());
    assert!(!ws.join(".forgeql.yaml").exists());

    let mut engine = engine_for(outer.path());
    let sid = engine
        .register_local_session(&ws)
        .expect("absence is the designed fallback and must still register");

    match common::execute_fql(&mut engine, &sid, "FIND symbols WHERE name = 'velocidad'") {
        ForgeQLResult::Query(qr) => assert!(
            qr.results.iter().any(|r| r.name == "velocidad"),
            "the in-memory backend answered nothing: {:?}",
            qr.results
        ),
        other => panic!("expected Query, got: {other:?}"),
    }
}
