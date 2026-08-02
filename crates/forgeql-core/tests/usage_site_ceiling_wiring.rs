//! `FIND usages` honours the site ceiling *through the engine*, at exactly the
//! value production is compiled with.
//!
//! `filter::tests` already proves the mechanism: an oversized first file comes
//! back whole, later files drop from the tail, and the response says so. It
//! proves it by calling `take_file_groups` directly with a small literal
//! ceiling, which is the right way to test the mechanism — but it says nothing
//! about the one line that connects the mechanism to production. Rewire
//! `exec_find` to a different constant, or a stale copy of it, and every one of
//! those tests stays green while a real query renders something else.
//!
//! So neither test here names a number; both size their fixtures from
//! `USAGE_SITE_CEILING` itself. They are deliberately a **pair**, because one
//! alone is one-sided:
//!
//! - the oversized-first-file case fails only if the wired ceiling is *larger*
//!   than production, since the first group renders whole whatever the ceiling;
//! - the exactly-fills case fails in *both* directions — a smaller wired
//!   ceiling withholds a file that must be rendered, a larger one admits a file
//!   that must not be.

#![allow(clippy::expect_used)]

mod common;

use common::{as_query, columnar_session_in};
use forgeql_core::filter::USAGE_SITE_CEILING;

/// The token every fixture references, once per line.
const PROBE: &str = "CEILING_PROBE";

/// Write `sites` references to [`PROBE`] into `name`, one per line.
///
/// `.cpp`, not `.c`: the shared test registry carries C++, Rust, Python and the
/// text formats, and a corpus of files it cannot index produces no segments at
/// all, which fails at session registration rather than returning zero rows.
///
/// File names are ordered `a_`/`b_`/`c_` on purpose. The overlay sorts segments
/// by source path (`overlay_builder`, where the sort key is marked load-bearing)
/// and `take_file_groups` groups in first-encounter order, so the prefixes fix
/// which file is the first group — the one rendered unconditionally.
fn write_sites(dir: &std::path::Path, name: &str, sites: usize) {
    let mut body = String::from("void probe(void) {\n");
    for _ in 0..sites {
        body.push_str("    ");
        body.push_str(PROBE);
        body.push_str(";\n");
    }
    body.push_str("}\n");
    std::fs::write(dir.join(name), body).expect("write fixture");
}

/// The first file is rendered whole even when it alone exceeds the ceiling.
///
/// Catches a call site wired *above* production: at any ceiling up to the first
/// file's own size the second file is withheld, so only a wired value of
/// `USAGE_SITE_CEILING + 2` or more would admit it.
#[test]
fn find_usages_renders_an_oversized_first_file_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_sites(dir.path(), "a_first.cpp", USAGE_SITE_CEILING + 1);
    write_sites(dir.path(), "b_second.cpp", 1);
    let mut session = columnar_session_in(dir);

    let result = session.exec(&format!("FIND usages OF '{PROBE}'"));
    let query = as_query(&result);

    assert_eq!(
        query.results.len(),
        USAGE_SITE_CEILING + 1,
        "the first file must come back complete — a response showing no file \
         answers nothing"
    );
    assert!(
        query.results.iter().all(|row| row
            .path
            .as_ref()
            .is_some_and(|p| p.ends_with("a_first.cpp"))),
        "only the first file is rendered; the second is withheld whole"
    );
    assert_eq!(
        query.total,
        USAGE_SITE_CEILING + 2,
        "total counts every site, including those in the withheld file"
    );
    assert!(
        query
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("files withheld"),
        "the response must say a file was withheld"
    );
}

/// Files that exactly fill the ceiling are all rendered; one more site is not.
///
/// This is the two-sided half. A wired ceiling below `USAGE_SITE_CEILING`
/// withholds `b_fills.cpp` and the row count drops; one above admits
/// `c_over.cpp` and the row count rises. Only the production value yields
/// exactly `USAGE_SITE_CEILING` rows with a withheld hint.
#[test]
fn find_usages_admits_exactly_the_production_ceiling() {
    let half = USAGE_SITE_CEILING / 2;
    let rest = USAGE_SITE_CEILING - half;

    let dir = tempfile::tempdir().expect("tempdir");
    write_sites(dir.path(), "a_head.cpp", half);
    write_sites(dir.path(), "b_fills.cpp", rest);
    write_sites(dir.path(), "c_over.cpp", 1);
    let mut session = columnar_session_in(dir);

    let result = session.exec(&format!("FIND usages OF '{PROBE}'"));
    let query = as_query(&result);

    assert_eq!(
        query.results.len(),
        USAGE_SITE_CEILING,
        "the two files that exactly fill the ceiling must both render, and the \
         third must not — a wired ceiling either side of production moves this"
    );
    assert!(
        query.results.iter().all(|row| row
            .path
            .as_ref()
            .is_some_and(|p| !p.ends_with("c_over.cpp"))),
        "the file that would overflow the ceiling is withheld whole"
    );
    assert_eq!(
        query.total,
        USAGE_SITE_CEILING + 1,
        "total counts the withheld file's site too"
    );
    assert!(
        query
            .hint
            .as_deref()
            .unwrap_or_default()
            .contains("files withheld"),
        "the response must say a file was withheld"
    );
}
