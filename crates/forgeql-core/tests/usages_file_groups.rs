//! `FIND usages` caps by file, not by row.
//!
//! A usage site is one line of one file, and the question behind the query is
//! "which files hold this name?".  A cap counted in rows answers a different
//! question: it cuts the list mid-file, so a file reports part of its sites and
//! hides the rest with nothing to say it did.  These tests pin the replacement
//! contract — whole file groups, ordered by their first site, every site of a
//! selected file returned, a `total` that stays the true site count, and a
//! `FOUND` set that arms only when no file was dropped.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

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
