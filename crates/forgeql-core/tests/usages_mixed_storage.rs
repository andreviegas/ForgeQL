//! `FIND usages` of a name its languages store differently.
//!
//! A header path is ONE token in C and C++ and three in anything that ends a
//! token at `/` and `.`, so one corpus holds the same name recorded both ways.
//! Answering from either tier alone drops every site the other one holds — the
//! C sites when the query is served from split parts, the prose sites when it
//! is served from the whole token — and a short list of sites reads exactly
//! like a complete one. These tests pin the merge, the line as the arbiter of
//! what counts as a site, and the answer for a name no part of which any
//! recorder could have stored.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use forgeql_core::result::{ForgeQLResult, QueryResult};

/// One token in C++; `zephyr`, `pm` and `device` anywhere else.
const NEEDLE: &str = "zephyr/pm/device.h";

/// One file per way the name can be stored, plus one that only looks like it.
fn mixed_storage_workspace() -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |name: &str, body: &str| {
        std::fs::write(dir.path().join(name), body).expect("write fixture");
    };

    // C++ records an include path as one token holding the whole path.
    write(
        "driver.cpp",
        "#include <zephyr/pm/device.h>\n\nvoid driver_init() {}\n",
    );
    // Rust does not widen its alphabet past `_`, so the same path written in a
    // comment is recorded as `zephyr`, `pm` and `device` — never as itself.
    write(
        "notes.rs",
        "// ported from zephyr/pm/device.h\npub fn notes() {}\n",
    );
    // Every part on one line, in an arrangement that is not the name.
    write(
        "decoy.rs",
        "// zephyr, pm and device.h are three separate things\npub fn decoy() {}\n",
    );
    // Nothing here can open a token, so no part can propose this line.
    write(
        "cryptic.rs",
        "// the a.b form is legacy\npub fn cryptic() {}\n",
    );

    common::columnar_session_in(dir)
}

fn query(t: &mut common::TestSession, fql: &str) -> QueryResult {
    match t.exec(fql) {
        ForgeQLResult::Query(q) => q,
        other => panic!("expected Query, got {other:?}"),
    }
}

/// Distinct file names among the returned rows.
fn files(q: &QueryResult) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for r in &q.results {
        let path = r
            .path
            .as_ref()
            .map_or_else(String::new, |p| p.to_string_lossy().into_owned());
        if !seen.contains(&path) {
            seen.push(path);
        }
    }
    seen
}

/// The whole-token tier and the split-parts tier answer the same query, and
/// the answer is their union. Neither alone can see the other's file.
#[test]
fn a_name_stored_whole_by_one_language_and_split_by_another_returns_both() {
    let mut t = mixed_storage_workspace();

    let found = files(&query(&mut t, &format!("FIND usages OF '{NEEDLE}'")));

    assert!(
        found.contains(&"driver.cpp".to_owned()),
        "the C++ include stored the whole path as one token: {found:?}"
    );
    assert!(
        found.contains(&"notes.rs".to_owned()),
        "the Rust comment stored only the parts, and the line holds the name: {found:?}"
    );
}

/// The source line is the arbiter, so a line carrying every part in some other
/// arrangement is not a site however loosely its parts proposed it.
#[test]
fn a_line_holding_the_parts_but_not_the_name_is_not_a_site() {
    let mut t = mixed_storage_workspace();

    let found = files(&query(&mut t, &format!("FIND usages OF '{NEEDLE}'")));

    assert!(
        !found.contains(&"decoy.rs".to_owned()),
        "`zephyr`, `pm` and `device.h` on one line are not `{NEEDLE}`: {found:?}"
    );
}

/// A name no part of which could ever have been a token leaves the index with
/// nothing to propose. The files are read instead — the sites exist, so the
/// answer is not zero — and the site says it came from the bytes.
#[test]
fn a_name_no_part_of_which_can_be_a_token_is_answered_by_reading_the_files() {
    let mut t = mixed_storage_workspace();

    let q = query(&mut t, "FIND usages OF 'a.b'");

    assert_eq!(
        files(&q),
        vec!["cryptic.rs".to_owned()],
        "the only line holding `a.b` is found by reading it"
    );
    assert_eq!(
        q.results.len(),
        1,
        "one site, and reading a file twice would report it twice"
    );
    assert_eq!(
        q.results[0].fields.get("role").map(String::as_str),
        Some("text"),
        "a scanned site cannot claim to know what kind of occurrence it is"
    );
}

/// The completeness claim cuts both ways: a name nothing holds still answers
/// zero, whichever tier looked for it.
#[test]
fn a_name_no_file_holds_answers_zero() {
    let mut t = mixed_storage_workspace();

    for absent in ["nowhere/at/all.h", "x.y"] {
        let q = query(&mut t, &format!("FIND usages OF '{absent}'"));
        assert!(
            q.results.is_empty(),
            "'{absent}' is in no file: {:?}",
            files(&q)
        );
    }
}
