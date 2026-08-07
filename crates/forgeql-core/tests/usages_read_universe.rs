//! Which files `FIND usages` reads, and which bytes inside them count.
//!
//! The answer comes from reading the workspace, so completeness is two claims:
//! one about the set of files, one about their bytes. Both have been wrong.
//!
//! The set: a file created in this session whose extension no plugin claims
//! produces no segment, and the persistent overlay's file-only entries are a
//! snapshot taken before the session began — so until the next commit it was
//! in nothing the read pass enumerated, and every query over it answered a
//! confident zero.
//!
//! The bytes: the text/binary line has a failure on each side. Decode
//! strictly and one byte in a legacy encoding rejects a whole file whose other
//! lines hold the name, silently, because the read itself succeeded. Decode
//! everything and a compiled object reports the ASCII of the symbol names it
//! embeds, which puts bytes no editor should rewrite into a `FOUND` set and
//! arms a sweep on them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use forgeql_core::result::{ForgeQLResult, QueryResult, ShowContent};

/// Long enough that no fixture holds it by accident.
const NEEDLE: &str = "k_sleep_forever";

/// One file per way a workspace file can reach — or fail to reach — the read
/// pass. Only `anchor.rs` is here to be indexed; the rest are here to be read.
fn read_universe_workspace() -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    let write = |name: &str, bytes: &[u8]| {
        std::fs::write(dir.path().join(name), bytes).expect("write fixture");
    };

    // An ordinary indexed file, so the session has symbols at all.
    write("anchor.rs", b"pub fn anchor() {}\n");

    // Text apart from one byte: `0xE9` is `e`-acute in Latin-1 and is not
    // valid UTF-8. The name is on a different line, and that line is text by
    // any reading.
    let mut legacy = b"// caf".to_vec();
    legacy.push(0xE9);
    legacy.extend_from_slice(b" notes\n// waits in k_sleep_forever\npub fn legacy() {}\n");
    write("legacy.rs", &legacy);

    // No plugin claims `.txt`, so this file produces no segment. It is the
    // control for the one below: if it is not found, non-indexed files are not
    // being read at all and the binary assertion would pass for the wrong
    // reason.
    write("notes.txt", b"see k_sleep_forever for the idle path\n");

    // A NUL among the first bytes, then the name in plain ASCII — what a
    // compiled object holding a symbol table looks like.
    let mut blob = vec![0x00, 0x01, 0x02];
    blob.extend_from_slice(b"k_sleep_forever\n");
    write("idle.o", &blob);

    // A declared UTF-16 document. Every ASCII character in it carries a NUL,
    // so without the mark it is indistinguishable from `idle.o` above.
    let mut wide = vec![0xFF, 0xFE];
    for unit in "// idle path uses k_sleep_forever\n".encode_utf16() {
        wide.extend_from_slice(&unit.to_le_bytes());
    }
    write("wide.txt", &wide);

    common::columnar_session_in(dir)
}

fn query(t: &mut common::TestSession, fql: &str) -> QueryResult {
    match t.exec(fql) {
        ForgeQLResult::Query(q) => q,
        other => panic!("expected Query, got {other:?}"),
    }
}

/// Run a mutation and assert it applied. A creation that quietly failed would
/// leave every assertion below passing for the wrong reason.
fn mutate(t: &mut common::TestSession, fql: &str) {
    match t.exec(fql) {
        ForgeQLResult::Mutation(m) => assert!(m.applied, "not applied: {fql}"),
        other => panic!("expected Mutation, got {other:?}"),
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

/// Paths from a `FIND files` listing, which answers as a `Show`, not a `Query`.
fn listed_files(t: &mut common::TestSession, fql: &str) -> Vec<String> {
    match t.exec(fql) {
        ForgeQLResult::Show(show) => match show.content {
            ShowContent::FileList { files, .. } => files
                .into_iter()
                .map(|entry| entry.path.to_string_lossy().into_owned())
                .collect(),
            other => panic!("expected a file list, got {other:?}"),
        },
        other => panic!("expected Show, got {other:?}"),
    }
}

/// A file created in this session that no plugin claims is in no committed
/// structure at all — not a segment, not a file-only entry. It is still on
/// disk, so it is still searched.
#[test]
fn a_non_indexed_file_created_in_this_session_is_searched_before_any_commit() {
    let mut t = read_universe_workspace();

    mutate(&mut t, "INSERT NODE FOR 'session.conf'");
    let handle = common::path_handle("session.conf");
    mutate(
        &mut t,
        &format!("INSERT AFTER NODE '{handle}' WITH 'CONFIG_IDLE={NEEDLE}'"),
    );

    let found = files(&query(&mut t, &format!("FIND usages OF '{NEEDLE}'")));

    assert!(
        found.contains(&"session.conf".to_owned()),
        "a file this session created holds the name on disk: {found:?}"
    );
}

/// `FIND files` and the read pass answer over the same universe, so a file one
/// of them can see and the other cannot is the defect either way round.
#[test]
fn a_non_indexed_file_created_in_this_session_is_listed_as_a_file() {
    let mut t = read_universe_workspace();

    mutate(&mut t, "INSERT NODE FOR 'session.conf'");
    let handle = common::path_handle("session.conf");
    mutate(
        &mut t,
        &format!("INSERT AFTER NODE '{handle}' WITH 'CONFIG_IDLE={NEEDLE}'"),
    );

    let listed = listed_files(&mut t, "FIND files LIMIT 50");

    assert!(
        listed.contains(&"session.conf".to_owned()),
        "the file exists in the workspace: {listed:?}"
    );
}

/// One byte in a legacy encoding must not blank the lines around it. Strict
/// decoding rejected the whole file and reported nothing, which reads exactly
/// like the name not being there.
#[test]
fn a_file_that_is_text_apart_from_one_byte_is_still_searched() {
    let mut t = read_universe_workspace();

    let found = files(&query(&mut t, &format!("FIND usages OF '{NEEDLE}'")));

    assert!(
        found.contains(&"legacy.rs".to_owned()),
        "the name is on a line that is plain ASCII: {found:?}"
    );
}

/// The other half of the same trade-off. A site here would arm a `MATCHING
/// WORD` sweep over bytes that are not source.
#[test]
fn a_file_whose_first_bytes_hold_a_nul_is_not_searched() {
    let mut t = read_universe_workspace();

    let found = files(&query(&mut t, &format!("FIND usages OF '{NEEDLE}'")));

    assert!(
        found.contains(&"notes.txt".to_owned()),
        "control: a non-indexed text file is read, so the next assertion is \
         about the NUL and not about the file being unreachable: {found:?}"
    );
    assert!(
        !found.contains(&"idle.o".to_owned()),
        "a NUL in the first bytes means these are not text: {found:?}"
    );
}

/// The mark is the difference between the two files above. `wide.txt` and
/// `idle.o` both hold NUL bytes; only one of them says what it is.
#[test]
fn a_file_that_declares_utf16_is_read_as_text() {
    let mut t = read_universe_workspace();

    let found = files(&query(&mut t, &format!("FIND usages OF '{NEEDLE}'")));

    assert!(
        found.contains(&"wide.txt".to_owned()),
        "a declared UTF-16 document holds the name on a line of its own: {found:?}"
    );
}

/// The other half, and the one that decides whether reading UTF-16 was safe to
/// do at all. Every edit writes UTF-8 bytes, and a line boundary in UTF-16 is
/// not a byte boundary, so a splice would shift the rest of the file. The
/// write must be refused, not attempted — and the bytes must be untouched
/// afterwards.
#[test]
fn an_edit_at_a_site_in_a_utf16_file_is_refused_and_writes_nothing() {
    let mut t = read_universe_workspace();
    let before = std::fs::read(t.workspace().join("wide.txt")).expect("fixture");

    let (handle, rev) = t.file_handle("wide.txt");
    let err = t.err(&format!(
        "CHANGE NODE '{handle}(1)' IF REV '{rev}' WITH 'renamed'"
    ));

    assert!(
        err.contains("UTF-16LE"),
        "the refusal must name the encoding — the site was just listed by a \
         query, so an unexplained error reads as a defect: {err}"
    );
    assert_eq!(
        std::fs::read(t.workspace().join("wide.txt")).expect("fixture"),
        before,
        "a refused write must leave the file exactly as it was"
    );
}

/// `COPY LINES` and `MOVE LINES` are the edit verbs for non-indexed files —
/// exactly the class the read pass now searches — and they reach a destination
/// through a different byte-offset calculation than the node verbs do. A guard
/// on one and not the other leaves the file corruptible by the other.
#[test]
fn copying_lines_into_a_utf16_destination_is_refused_and_writes_nothing() {
    let mut t = read_universe_workspace();
    let before = std::fs::read(t.workspace().join("wide.txt")).expect("fixture");

    let err = t.err("COPY LINES 1-1 OF 'notes.txt' TO 'wide.txt'");

    assert!(
        err.contains("UTF-16LE"),
        "the destination is UTF-16 and the payload is UTF-8: {err}"
    );
    assert_eq!(
        std::fs::read(t.workspace().join("wide.txt")).expect("fixture"),
        before,
        "a refused copy must leave the destination exactly as it was"
    );
}

/// `MOVE LINES` reaches a cross-file destination through its own branch, not
/// through the one `COPY LINES` uses. The defect being fixed was "a destination
/// reached by a separate offset calculation", so each calculation needs its own
/// pin or the next one added comes back unguarded.
#[test]
fn moving_lines_into_a_utf16_destination_is_refused_and_writes_nothing() {
    let mut t = read_universe_workspace();
    let before = std::fs::read(t.workspace().join("wide.txt")).expect("fixture");
    let source_before = std::fs::read(t.workspace().join("notes.txt")).expect("fixture");

    let err = t.err("MOVE LINES 1-1 OF 'notes.txt' TO 'wide.txt'");

    assert!(
        err.contains("UTF-16LE"),
        "the destination is UTF-16 and the payload is UTF-8: {err}"
    );
    assert_eq!(
        std::fs::read(t.workspace().join("wide.txt")).expect("fixture"),
        before,
        "a refused move must leave the destination exactly as it was"
    );
    assert_eq!(
        std::fs::read(t.workspace().join("notes.txt")).expect("fixture"),
        source_before,
        "and must not have removed the lines from the source either"
    );
}

/// Replacing the whole file is refused too, so the docs must not offer it as
/// the way to convert one. A whole-file `CHANGE NODE` is lowered to a line
/// range, and a line range in UTF-16 does not even cover the file: scanning for
/// `0x0A` finds the newline byte but stops one byte short of the code unit it
/// belongs to, so the "whole file" it computes is missing its last byte.
#[test]
fn replacing_a_utf16_file_whole_is_refused_as_well() {
    let mut t = read_universe_workspace();
    let before = std::fs::read(t.workspace().join("wide.txt")).expect("fixture");

    let (handle, rev) = t.file_handle("wide.txt");
    let err = t.err(&format!(
        "CHANGE NODE '{handle}' IF REV '{rev}' WITH 'converted to utf-8'"
    ));

    assert!(
        err.contains("UTF-16LE"),
        "a whole-file replacement is a line range like any other: {err}"
    );
    assert_eq!(
        std::fs::read(t.workspace().join("wide.txt")).expect("fixture"),
        before,
        "a refused replacement must leave the file exactly as it was"
    );
}

/// The route that does work in place, and the reason it is safe: it replaces
/// every byte, so no half of the file is left in the other encoding. It is
/// `CHANGE FILE`, which is refused on indexed files and therefore available on
/// exactly the non-indexed ones the read pass reaches — and if the guard ever
/// spread to it, converting a UTF-16 file through the DSL at all would stop
/// being possible. That is what this pins.
#[test]
fn a_utf16_file_can_be_converted_by_replacing_every_byte() {
    let mut t = read_universe_workspace();

    mutate(
        &mut t,
        "CHANGE FILE 'wide.txt' WITH 'idle path uses k_sleep_forever'",
    );

    let after = std::fs::read(t.workspace().join("wide.txt")).expect("fixture");
    assert!(
        !after.starts_with(&[0xFF, 0xFE]),
        "the byte-order mark is gone: {after:?}"
    );
    assert!(
        !after.contains(&0x00),
        "and so are the NUL bytes that made it unreadable as text: {after:?}"
    );
    assert_eq!(
        String::from_utf8_lossy(&after).trim_end(),
        "idle path uses k_sleep_forever"
    );
}

/// A directory is not a file the read pass can open. Recording one would make
/// every later query report an unreadable file that is not missing anything,
/// and list the directory without the trailing slash that marks it.
#[test]
fn creating_a_directory_does_not_add_it_to_the_files_that_are_read() {
    let mut t = read_universe_workspace();

    mutate(&mut t, "INSERT NODE FOR 'generated/'");

    let q = query(&mut t, &format!("FIND usages OF '{NEEDLE}'"));
    assert!(
        !files(&q).contains(&"generated".to_owned()),
        "a directory holds no lines: {:?}",
        files(&q)
    );
    assert!(
        q.hint.as_deref().unwrap_or_default().is_empty(),
        "nothing was unreadable, so nothing may claim it was: {:?}",
        q.hint
    );
}
