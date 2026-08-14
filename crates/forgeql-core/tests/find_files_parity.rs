//! `FIND files` served from the stored union must agree with the worktree.
//!
//! The stored path answers every query shape from the overlay's own file
//! list (indexed segments, file-only entries, dirty adds) with directory
//! rows derived from it — no filesystem walk. These tests pin the
//! acceptance contract: on a workspace holding indexed files, non-indexed
//! files, session-created files, session-modified files and files deleted
//! in-session, the served rows and the served total must match the
//! filesystem exactly. A wrong row here is a silent false negative.
//!
//! The test harness places the engine's own storage (`data/`, `segments/`,
//! `overlays/`) inside the workspace root, where the storage layer writes
//! behind the engine's back; every query and every reference walk excludes
//! those three trees so the comparison covers only real workspace files.
//!
//! Run with: `cargo test -p forgeql-core --test find_files_parity`
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    // panic! is the normal way to fail a test assertion
    clippy::panic,
    // helper functions defined inside test bodies after let-statements
    clippy::items_after_statements
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use forgeql_core::result::{FileEntry, ForgeQLResult, ShowContent, ShowResult};

mod common;

use common::{columnar_session_in, execute_fql};

const EXCLUDES: &str = "EXCLUDE 'data/**' EXCLUDE 'segments/**' EXCLUDE 'overlays/**'";

fn is_harness_storage(rel: &str) -> bool {
    ["data", "segments", "overlays"]
        .iter()
        .any(|d| rel == *d || rel.starts_with(&format!("{d}/")))
}

/// Build the workspace, register a columnar session over it, then mutate it
/// through the engine into every state the stored list must cover.
fn parity_session() -> (
    forgeql_core::engine::ForgeQLEngine,
    String,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    fs::create_dir_all(dir.path().join("assets")).unwrap();
    fs::write(
        dir.path().join("src/alpha.cpp"),
        "int alpha_fn() { return 1; }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("src/deep/beta.cpp"),
        "int beta_fn() { return 2; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("assets/nested")).unwrap();
    fs::write(dir.path().join("assets/data.bin"), b"\x00\x01\x02\x03").unwrap();
    fs::write(
        dir.path().join("assets/nested/deep.bin"),
        b"\x01\x02\x03\x04\x05",
    )
    .unwrap();
    fs::write(dir.path().join("assets/gone.bin"), b"\xff\xfe").unwrap();

    let (mut engine, sid, dir) = columnar_session_in(dir).into_parts();

    // Deleted in-session: one indexed file, one non-indexed file.
    for rel in ["src/deep/beta.cpp", "assets/gone.bin"] {
        let handle = common::path_handle(rel);
        let rev = common::node_rev(&mut engine, &sid, &handle);
        let _ = execute_fql(
            &mut engine,
            &sid,
            &format!("DELETE NODE '{handle}' IF REV '{rev}'"),
        );
    }
    // Created in-session: an indexed file with content, an empty non-indexed
    // file, and an empty directory.
    let _ = execute_fql(&mut engine, &sid, "INSERT NODE FOR 'made/gamma.cpp'");
    let gamma = common::path_handle("made/gamma.cpp");
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("INSERT AFTER NODE '{gamma}' WITH 'int gamma_fn() {{ return 3; }}'"),
    );
    let _ = execute_fql(&mut engine, &sid, "INSERT NODE FOR 'made/notes.bin'");
    let _ = execute_fql(&mut engine, &sid, "INSERT NODE FOR 'made/empty/'");
    // Modified in-session: the surviving indexed file becomes a dirty segment.
    let alpha = common::path_handle("src/alpha.cpp");
    let rev = common::node_rev(&mut engine, &sid, &alpha);
    let _ = execute_fql(
        &mut engine,
        &sid,
        &format!("CHANGE NODE '{alpha}' IF REV '{rev}' MATCHING 'return 1' WITH 'return 42'"),
    );
    (engine, sid, dir)
}

fn file_list(
    engine: &mut forgeql_core::engine::ForgeQLEngine,
    sid: &str,
    fql: &str,
) -> (Vec<FileEntry>, usize) {
    match execute_fql(engine, sid, fql) {
        ForgeQLResult::Show(ShowResult {
            content: ShowContent::FileList { files, total },
            ..
        }) => (files, total),
        other => panic!("expected FileList, got {other:?}"),
    }
}

/// The reference: what is actually on disk, post-mutations — mutations write
/// through to the worktree, so the filesystem IS the expected universe.
fn walk_reference(root: &Path) -> (BTreeMap<String, u64>, BTreeMap<String, (u64, usize)>) {
    let mut files: BTreeMap<String, u64> = BTreeMap::new();
    let mut dirs: BTreeMap<String, (u64, usize)> = BTreeMap::new();
    fn visit(root: &Path, dir: &Path, files: &mut BTreeMap<String, u64>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".forgeql") || name == ".git" {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                if !is_harness_storage(&rel) {
                    let _ = files.insert(rel, fs::metadata(&path).unwrap().len());
                }
            }
        }
    }
    visit(root, root, &mut files);
    for (rel, size) in &files {
        let mut anc = PathBuf::from(rel);
        while let Some(parent) = anc.parent().map(Path::to_path_buf) {
            if parent.as_os_str().is_empty() {
                break;
            }
            let slot = dirs
                .entry(parent.to_string_lossy().into_owned())
                .or_default();
            slot.0 += size;
            slot.1 += 1;
            anc = parent;
        }
    }
    (files, dirs)
}

#[test]
fn the_stored_list_matches_the_worktree_exactly_across_every_session_state() {
    let (mut engine, sid, dir) = parity_session();
    // The explicit LIMIT clears the default page: this test wants the whole
    // universe, and the total must not depend on the bound.
    let (rows, total) = file_list(
        &mut engine,
        &sid,
        &format!("FIND files {EXCLUDES} LIMIT 100000"),
    );

    let (want_files, want_dirs) = walk_reference(dir.path());

    let mut got_files: BTreeMap<String, u64> = BTreeMap::new();
    let mut got_dirs: BTreeMap<String, (u64, usize)> = BTreeMap::new();
    for row in &rows {
        let p = row.path.to_string_lossy().into_owned();
        if let Some(d) = p.strip_suffix('/') {
            let prior = got_dirs.insert(d.to_owned(), (row.size, row.count.unwrap_or(0)));
            assert!(prior.is_none(), "directory listed twice: {d}");
        } else {
            let prior = got_files.insert(p.clone(), row.size);
            assert!(prior.is_none(), "file listed twice: {p}");
        }
    }

    // Files: identical set, identical sizes. Deleted files must be absent,
    // created and modified ones present at their current size.
    assert_eq!(got_files, want_files, "file rows must equal the worktree");
    assert!(!got_files.contains_key("src/deep/beta.cpp"));
    assert!(!got_files.contains_key("assets/gone.bin"));
    assert!(got_files.contains_key("made/notes.bin"));

    // Directories: one row per non-empty directory; size is the bytes of the
    // files beneath it (any depth), count how many they are. The empty
    // directory created this session is addressable by its returned handle
    // but not listed — git cannot commit one, so it is session-transient.
    assert_eq!(got_dirs, want_dirs, "dir rows must aggregate the worktree");
    assert!(!got_dirs.contains_key("made/empty"));

    // The total is the whole answer: rows are uncapped here, so it is the row
    // count, and every row was accounted for above.
    assert_eq!(total, rows.len());
}

#[test]
fn the_scoped_and_filtered_shapes_serve_the_same_universe() {
    let (mut engine, sid, _dir) = parity_session();

    // IN glob: only src files and src directories, nothing else.
    let (rows, total) = file_list(&mut engine, &sid, "FIND files IN 'src/**'");
    let got: Vec<String> = rows
        .iter()
        .map(|r| r.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(total, rows.len());
    assert!(got.contains(&"src/alpha.cpp".to_owned()), "{got:?}");
    assert!(got.contains(&"src/".to_owned()), "{got:?}");
    assert!(
        got.iter().all(|p| p.starts_with("src")),
        "IN must scope every row: {got:?}"
    );
    assert!(
        !got.contains(&"src/deep/beta.cpp".to_owned()),
        "a deleted file must not resurface under IN: {got:?}"
    );

    // WHERE extension: files of that extension only (a directory has none).
    let (rows, _) = file_list(
        &mut engine,
        &sid,
        &format!("FIND files {EXCLUDES} WHERE extension = 'bin'"),
    );
    let mut got: Vec<String> = rows
        .iter()
        .map(|r| r.path.to_string_lossy().into_owned())
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "assets/data.bin".to_owned(),
            "assets/nested/deep.bin".to_owned(),
            "made/notes.bin".to_owned()
        ]
    );

    // WHERE path LIKE '%/': the documented directory-list recipe still works.
    // Three directories hold files after the mutations — src/deep was
    // emptied by the delete and made/empty never held one, and a directory
    // row exists exactly when files lie beneath it.
    let (rows, _) = file_list(
        &mut engine,
        &sid,
        &format!("FIND files {EXCLUDES} WHERE path LIKE '%/'"),
    );
    let mut got: Vec<String> = rows
        .iter()
        .map(|r| r.path.to_string_lossy().into_owned())
        .collect();
    got.sort();
    assert_eq!(
        got,
        vec![
            "assets/".to_owned(),
            "assets/nested/".to_owned(),
            "made/".to_owned(),
            "src/".to_owned()
        ]
    );
}

#[test]
fn depth_one_lists_each_directory_once_with_deep_survivor_aggregates() {
    let (mut engine, sid, _dir) = parity_session();
    let (rows, _) = file_list(&mut engine, &sid, &format!("FIND files {EXCLUDES} DEPTH 1"));

    let mut seen: Vec<String> = Vec::new();
    for row in &rows {
        let p = row.path.to_string_lossy().into_owned();
        assert!(!seen.contains(&p), "no path appears twice under DEPTH: {p}");
        seen.push(p);
    }
    // Depth grouping runs after WHERE and aggregates only the files DEEPER
    // than the cut: assets/nested/deep.bin is the one such file, so assets/
    // is the one directory row, carrying that file alone. Shallow files stay
    // individual rows and are never re-counted into an ancestor here.
    let dir_rows: Vec<&FileEntry> = rows
        .iter()
        .filter(|r| r.path.to_string_lossy().ends_with('/'))
        .collect();
    assert_eq!(dir_rows.len(), 1, "one aggregating directory: {seen:?}");
    assert_eq!(dir_rows[0].path.to_string_lossy(), "assets/");
    assert_eq!(dir_rows[0].size, 5, "deep survivor bytes only");
    assert_eq!(dir_rows[0].count, Some(1), "deep survivor count only");
    assert!(seen.contains(&"assets/data.bin".to_owned()), "{seen:?}");
}

#[test]
fn without_a_limit_find_files_serves_the_standard_page_with_an_honest_total() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("anchor.cpp"),
        "int anchor() { return 0; }\n",
    )
    .unwrap();
    fs::create_dir_all(dir.path().join("many")).unwrap();
    for i in 0..30 {
        fs::write(dir.path().join(format!("many/f{i:02}.bin")), b"x").unwrap();
    }
    let (mut engine, sid, _dir) = columnar_session_in(dir).into_parts();

    // 20 rows delivered, every matching row counted: truncation is visible
    // and OFFSET pages the rest. The absence of a bound never again means
    // "materialise everything".
    let (rows, total) = file_list(&mut engine, &sid, "FIND files IN 'many/**'");
    assert_eq!(rows.len(), 20, "the standard FIND page");
    assert_eq!(total, 31, "the whole answer counted: 30 files + many/");
}

#[test]
fn a_master_rev_armed_from_served_rows_verifies_against_a_fresh_lookup() {
    let (mut engine, sid, _dir) = parity_session();
    // A complete FIND files arms its master rev from the revs on the rows it
    // just served — including a DIRECTORY member, whose fresh lookup at
    // verify time takes the worktree resolver. The two derivations must
    // agree, or every FOUND verb on such a set would refuse with a false
    // rev_mismatch. The MOVE below must be refused for being a directory —
    // the check that runs AFTER the master rev verified.
    let r = execute_fql(
        &mut engine,
        &sid,
        &format!("FIND files {EXCLUDES} WHERE path LIKE 'src%' LIMIT 1000"),
    );
    let ForgeQLResult::Show(show) = r else {
        panic!("expected Show result");
    };
    let rev = show
        .metadata
        .as_ref()
        .and_then(|m| m.get("found_rev"))
        .and_then(|v| v.as_str())
        .expect("a complete FIND issues a master rev")
        .to_owned();
    let err = common::try_fql(
        &mut engine,
        &sid,
        &format!("MOVE NODES FOUND IF REV '{rev}' TO 'elsewhere/'"),
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("is a directory"),
        "the master rev must verify (a mismatch would surface first): {err}"
    );
}

#[test]
fn a_complete_listing_takes_the_same_route_as_a_truncated_one() {
    // Directory-heavy on purpose: the historical cliff was a complete
    // listing re-deriving every member at arming time, where each DIRECTORY
    // member's lookup walks the whole worktree — O(dirs × workspace).
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("anchor.cpp"),
        "int anchor() { return 0; }\n",
    )
    .unwrap();
    for i in 0..2000 {
        let d = dir.path().join(format!("blob/d{i:04}"));
        fs::create_dir_all(&d).unwrap();
        for j in 0..5 {
            fs::write(d.join(format!("f{j}.bin")), b"x").unwrap();
        }
    }
    let (mut engine, sid, _dir) = columnar_session_in(dir).into_parts();

    let started = std::time::Instant::now();
    let (rows, total) = file_list(&mut engine, &sid, "FIND files IN 'blob/**' LIMIT 12000");
    let truncated = started.elapsed();
    assert_eq!(total, 12_001, "10,000 files + 2,001 directories");
    assert_eq!(rows.len(), 12_000, "one short of complete: no master rev");

    let started = std::time::Instant::now();
    let (rows, _) = file_list(&mut engine, &sid, "FIND files IN 'blob/**' LIMIT 100000");
    let complete = started.elapsed();
    assert_eq!(rows.len(), 12_001);
    // The absence of truncation must not select a different route: the
    // complete listing arms its master rev from the rows it just served, so
    // its cost is the truncated cost plus one row — never a per-member
    // re-derive. The margin is generous to absorb shared-runner noise; the
    // defect this pins was a >25x cliff.
    assert!(
        complete < truncated * 3 + std::time::Duration::from_secs(2),
        "complete {complete:?} vs truncated {truncated:?}"
    );
}
