//! `FIND usages` caps by file, not by row.
//!
//! A usage site is one line of one file, and the question behind the query is
//! "which files hold this name?".  A cap counted in rows answers a different
//! question: it cuts the list mid-file, so a file reports part of its sites and
//! hides the rest with nothing to say it did.  These tests pin the replacement
//! contract — whole file groups, ordered by their first site, every site of a
//! selected file returned, a `total` that stays the true site count, and a
//! `FOUND` set that arms only when no file was dropped.
//!
//! They pin what the cap produces, not where it is applied.  The backend is
//! handed the file bound and cuts the page from the site list, so a site in a
//! file nobody will see is never built into a row — but the caller-side
//! selection this replaced produced the same rows and the same `total`, so no
//! case here can tell the two apart.  Nor does anything else pin it exactly:
//! the nearest is `a_bounded_page_renders_whole_files_and_still_counts_the_rest`
//! in `crates/forgeql-core/src/storage/mod.rs`, which asks for zero files and
//! gets zero rows delivered with the count intact — a regression that built all
//! five rows and then discarded them would satisfy it too.  The count of rows
//! BUILT is held by construction: one call site, mapping over the selection.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;
use std::fmt::Write as _;

use forgeql_core::result::{ForgeQLResult, QueryResult};

/// The name every fixture file references; unique enough not to collide with
/// anything the C++ grammar or the standard headers introduce.
const MARKER: &str = "probe_marker_fn";

/// Three files whose first-site order (zulu, alpha, mike) is neither
/// alphabetical nor creation order, so a fallback to path order or to segment
/// iteration order fails the ordering assert rather than passing by luck.
fn three_file_workspace() -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |name: &str, body: String| {
        std::fs::write(dir.path().join(name), body).expect("write fixture");
    };
    // First site on line 1.
    write(
        "zulu.cpp",
        format!("void {MARKER}();\n\nvoid zulu_use() {{ {MARKER}(); }}\n"),
    );
    // First site on line 5.
    write(
        "alpha.cpp",
        format!("//\n//\n//\n//\nvoid {MARKER}();\n\nvoid alpha_use() {{ {MARKER}(); }}\n"),
    );
    // First site on line 9.
    write(
        "mike.cpp",
        format!("//\n//\n//\n//\n//\n//\n//\n//\nvoid {MARKER}();\n"),
    );
    common::columnar_session_in(dir)
}

fn query(t: &mut common::TestSession, fql: &str) -> QueryResult {
    match t.exec(fql) {
        ForgeQLResult::Query(q) => q,
        other => panic!("expected Query, got {other:?}"),
    }
}

/// `(path, line)` for every returned row, in the order they were returned.
fn sites(q: &QueryResult) -> Vec<(String, usize)> {
    q.results
        .iter()
        .map(|r| {
            let path = r
                .path
                .as_ref()
                .map_or_else(String::new, |p| p.to_string_lossy().into_owned());
            (path, r.line.unwrap_or(0))
        })
        .collect()
}

/// Distinct paths in first-appearance order.
fn files_in_order(q: &QueryResult) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for (path, _) in sites(q) {
        if !seen.contains(&path) {
            seen.push(path);
        }
    }
    seen
}

/// Files come back ordered by their first site, each file's sites are
/// contiguous and ascending.  Deterministic: rerunning the same query on the
/// same workspace must not reshuffle it.
#[test]
fn file_order_follows_first_site_and_sites_ascend_within_a_file() {
    let mut t = three_file_workspace();
    let q = query(&mut t, &format!("FIND usages OF '{MARKER}'"));

    assert_eq!(
        files_in_order(&q),
        vec!["zulu.cpp", "alpha.cpp", "mike.cpp"],
        "files order by first site line (1, 5, 9), not by path"
    );

    let ordered: Vec<String> = sites(&q).into_iter().map(|(path, _)| path).collect();
    let mut runs = ordered.clone();
    runs.dedup();
    assert_eq!(
        runs.len(),
        3,
        "each file's sites must be contiguous, got {ordered:?}"
    );

    for file in files_in_order(&q) {
        let lines: Vec<usize> = sites(&q)
            .into_iter()
            .filter(|(path, _)| *path == file)
            .map(|(_, line)| line)
            .collect();
        assert!(
            lines.windows(2).all(|w| w[0] < w[1]),
            "{file}: sites must ascend, got {lines:?}"
        );
    }

    let again = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    assert_eq!(sites(&q), sites(&again), "the same query must be stable");
}

/// An explicit `LIMIT` selects files.  The selected file keeps every one of its
/// sites, and `total` still reports every site in the workspace.
#[test]
fn explicit_limit_counts_files_and_total_stays_complete() {
    let mut t = three_file_workspace();
    let all = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    let zulu = query(&mut t, &format!("FIND usages OF '{MARKER}' IN 'zulu.cpp'"));
    assert!(zulu.results.len() > 1, "fixture needs a multi-site file");

    let capped = query(&mut t, &format!("FIND usages OF '{MARKER}' LIMIT 1"));
    assert_eq!(files_in_order(&capped), vec!["zulu.cpp"]);
    assert_eq!(
        capped.results.len(),
        zulu.results.len(),
        "every site of the one selected file"
    );
    assert_eq!(
        capped.total, all.total,
        "total stays the true site count under an explicit LIMIT"
    );
    assert!(
        capped.total > capped.results.len(),
        "the dropped files are visible as the gap between total and rows"
    );
}

/// `OFFSET` skips whole files, so paging never splits one file across pages.
#[test]
fn offset_skips_whole_files() {
    let mut t = three_file_workspace();
    let page_two = query(
        &mut t,
        &format!("FIND usages OF '{MARKER}' LIMIT 1 OFFSET 1"),
    );
    assert_eq!(files_in_order(&page_two), vec!["alpha.cpp"]);
    let alpha = query(&mut t, &format!("FIND usages OF '{MARKER}' IN 'alpha.cpp'"));
    assert_eq!(page_two.results.len(), alpha.results.len());
}

/// The default cap counts files too: 20 files come back whole out of 22.
#[test]
fn default_limit_counts_files_not_sites() {
    let dir = tempfile::tempdir().expect("tempdir");
    for i in 0..22 {
        // Unique bodies: byte-identical files would share an index segment.
        std::fs::write(
            dir.path().join(format!("f{i:02}.cpp")),
            format!("void {MARKER}();\n\nvoid use_{i:02}() {{ {MARKER}(); }}\n"),
        )
        .expect("write fixture");
    }
    let mut t = common::columnar_session_in(dir);

    let per_file = query(&mut t, &format!("FIND usages OF '{MARKER}' IN 'f00.cpp'"))
        .results
        .len();
    assert!(per_file > 1, "fixture needs several sites per file");

    let q = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    assert_eq!(files_in_order(&q).len(), 20, "the default cap is 20 files");
    assert_eq!(q.results.len(), per_file * 20, "each of them complete");
    assert_eq!(q.total, per_file * 22, "total counts all 22 files' sites");
}

/// `GROUP BY` keeps its own shape: its rows are aggregates, already one per
/// group, and its `LIMIT` counts those groups.
#[test]
fn group_by_is_exempt_from_the_file_cap() {
    let mut t = three_file_workspace();
    let q = query(
        &mut t,
        &format!("FIND usages OF '{MARKER}' GROUP BY file ORDER BY count DESC LIMIT 2"),
    );
    assert_eq!(q.results.len(), 2, "LIMIT counts groups under GROUP BY");
    assert!(
        q.results.iter().all(|r| r.count.is_some()),
        "aggregate rows carry a per-group count"
    );
}

/// `FOUND` arms only from a set the agent actually saw.  A result with every
/// file in it is complete and gets a master rev; one with files dropped by the
/// cap gets none, so every `FOUND` verb refuses.
#[test]
fn found_set_arms_only_when_no_file_was_dropped() {
    let mut t = three_file_workspace();

    let complete = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    assert_eq!(complete.results.len(), complete.total);
    assert!(
        complete.found_rev.is_some(),
        "a complete site set arms FOUND"
    );

    let capped = query(&mut t, &format!("FIND usages OF '{MARKER}' LIMIT 1"));
    assert!(capped.results.len() < capped.total);
    assert!(
        capped.found_rev.is_none(),
        "a set with files dropped must not arm FOUND"
    );
}

/// Every site row carries the handle and rev of the file it sits in — the two
/// values `CHANGE NODE '<file>(<line>)' IF REV '<rev>'` needs — and they are
/// the same values `FIND files` reports for that file.
#[test]
fn usage_rows_carry_their_file_handle_and_rev() {
    let mut t = three_file_workspace();
    let q = query(&mut t, &format!("FIND usages OF '{MARKER}' IN 'zulu.cpp'"));
    assert!(!q.results.is_empty());

    let (handle, rev) = t.file_handle("zulu.cpp");
    for row in &q.results {
        assert_eq!(row.node_id.as_deref(), Some(handle.as_str()));
        assert_eq!(row.rev.as_deref(), Some(rev.as_str()));
    }
}

/// The handle rides on the *rendered row*, never on the `FOUND` member.
///
/// A FOUND member is routed by its handle when it has one, and a file handle
/// resolves to the file's whole span — so a stamped member would turn a
/// `CHANGE NODES FOUND` sweep into a whole-file rewrite. The member stays what
/// it has always been: a path and one line.
#[test]
fn found_members_stay_line_scoped_when_rows_carry_a_handle() {
    let mut t = three_file_workspace();
    let q = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    assert!(
        q.results.iter().all(|r| r.node_id.is_some()),
        "the rendered rows do carry handles"
    );

    let set = forgeql_core::session::found_set::try_restore(t.workspace())
        .expect("a complete usages result arms FOUND");
    assert!(!set.members.is_empty());
    for member in &set.members {
        assert!(
            member.node_id.is_none(),
            "a usage site is a line, not a node: {member:?}"
        );
        assert!(member.line.is_some_and(|l| l >= 1), "{member:?}");
    }
}

/// The ceiling constant is actually wired into `find_usages`, and it drops
/// **whole files, from the tail** — file order never changes, and no file is
/// ever rendered partially.
#[test]
fn site_ceiling_is_wired_in_and_drops_whole_files() {
    // Three files of 900 sites each: the first two fit under the ceiling, the
    // third would carry the response past it.
    let dir = tempfile::tempdir().expect("tempdir");
    for f in 0..3 {
        let mut body = String::new();
        for i in 0..900 {
            let _ = writeln!(body, "void f{f}_{i:03}() {{ {MARKER}(); }}");
        }
        std::fs::write(dir.path().join(format!("bulk_{f}.cpp")), body).expect("write");
    }
    let mut t = common::columnar_session_in(dir);

    let q = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    let shown = files_in_order(&q);
    assert!(
        shown.len() < 3,
        "the ceiling must withhold at least one file, showed {shown:?}"
    );
    assert!(!shown.is_empty(), "at least one file is always shown");

    // Whole files only: every shown file carries all of its sites.
    for file in &shown {
        let got = sites(&q).into_iter().filter(|(p, _)| p == file).count();
        let alone = query(&mut t, &format!("FIND usages OF '{MARKER}' IN '{file}'"));
        assert_eq!(got, alone.results.len(), "{file} rendered whole");
    }

    assert!(q.total > q.results.len(), "total still counts every site");
    let hint = q.hint.expect("withheld files must be announced");
    assert!(
        hint.contains("withheld"),
        "the ceiling hint must say files were withheld: {hint}"
    );
}

/// However large the first file is, it is always rendered complete: a listing
/// that shows no file at all answers nothing.
#[test]
fn the_first_file_is_always_rendered_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut huge = String::new();
    for i in 0..300 {
        let _ = writeln!(huge, "void huge_{i:03}() {{ {MARKER}(); }}");
    }
    std::fs::write(dir.path().join("only.cpp"), huge).expect("write");
    let mut t = common::columnar_session_in(dir);

    let q = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    assert_eq!(files_in_order(&q), vec!["only.cpp"]);
    assert_eq!(
        q.results.len(),
        q.total,
        "the one selected file keeps every site"
    );
    assert!(q.results.len() >= 300, "got {} sites", q.results.len());
}

/// When files are withheld the response says so, and says which lever helps:
/// past the ceiling a bigger LIMIT changes nothing.
#[test]
fn withholding_files_is_announced_in_a_hint() {
    let mut t = three_file_workspace();

    let complete = query(&mut t, &format!("FIND usages OF '{MARKER}'"));
    assert!(
        complete.hint.is_none(),
        "a complete listing needs no hint: {:?}",
        complete.hint
    );

    let capped = query(&mut t, &format!("FIND usages OF '{MARKER}' LIMIT 1"));
    let hint = capped.hint.expect("a file-capped listing must say so");
    assert!(hint.contains("LIMIT"), "{hint}");
    assert!(hint.contains("total"), "{hint}");
}
