//! Pattern predicates answered from a field's DISTINCT VALUES, not its rows.
//!
//! `WHERE <posted field> MATCHES/LIKE '…'` and `WHERE name MATCHES '…'` used to
//! skip every index tier and materialise the whole corpus so the residual
//! filter could reject rows one at a time. Evaluating the pattern against the
//! values instead costs one test per distinct value — but the candidate set it
//! produces must still hold every row the residual filter would accept, and
//! that is what these tests pin.
//!
//! They build a real two-segment overlay because the completeness risk is a
//! per-segment one: the overlay's per-value bitmaps are assembled from each
//! segment's postings, and a segment whose distinct-value count for a field
//! exceeded the per-segment budget contributes none at all. Its rows still
//! carry the field, so intersecting the candidate set with such a bitmap drops
//! them from the answer — silently, and for `Eq` just as much as for a regex.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

mod overlay_harness;

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::ColumnarStorage;
use forgeql_core::storage::columnar::overlay::Overlay;
use overlay_harness::*;

/// Ten preprocessor arms in one file: `guard_branch` takes ten distinct values
/// there, past the per-segment posting budget of eight, so that segment ends up
/// storing the column and no postings for it.
fn many_arms() -> String {
    let mut src = String::from("#if defined(ARM_0)\nint arm_0;\n");
    for i in 1..10 {
        let _ = write!(src, "#elif defined(ARM_{i})\nint arm_{i};\n");
    }
    src.push_str("#endif\n");
    src
}

/// Two arms — well inside the budget, so this file DOES post `guard_branch`,
/// and the values it posts overlap the ten-arm file's.
const TWO_ARMS: &str = "#if defined(OPT_A)\nint two_a;\n#else\nint two_b;\n#endif\n";

/// A real multi-segment workspace: `find_symbols` here runs the production
/// columnar pipeline — global prefilter, per-segment postings, materialise,
/// residual filter.
fn workspace(files: &[(&str, String)]) -> (TempDir, ColumnarStorage) {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("work");
    fs::create_dir_all(&root).expect("create worktree root");
    let segments_dir = tmp.path().join("segments");

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    for (name, body) in files {
        let abs = root.join(name);
        fs::write(&abs, body).expect("write fixture");
        let table = index_at_path(&CppLanguage, &abs);
        let cid = build_segment(&table, &abs, &segments_dir);
        let _ = segment_map.insert(abs, cid);
    }

    let overlay_path = tmp.path().join("overlays").join("test").join("w.bin");
    OverlayBuilder::new("test", segments_dir.clone(), root.clone(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_path(&segments_dir, &m.source_path, &m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    let storage =
        ColumnarStorage::new_unshared(root, segs, overlay, Arc::new(LanguageRegistry::new(vec![])));
    (tmp, storage)
}

/// The guard fixtures, as two segments of one workspace.
fn guard_workspace() -> (TempDir, ColumnarStorage) {
    workspace(&[
        ("a_many_arms.cpp", many_arms()),
        ("b_two_arms.cpp", TWO_ARMS.to_owned()),
    ])
}

/// Sorted names returned for one WHERE predicate.
fn found(storage: &ColumnarStorage, field: &str, op: CompareOp, value: &str) -> Vec<String> {
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: field.to_owned(),
            op,
            value: PredicateValue::String(value.to_owned()),
        }],
        ..Clauses::default()
    };
    let mut names: Vec<String> = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("find_symbols")
        .into_iter()
        .map(|m| m.name)
        .collect();
    names.sort();
    names
}

/// Ground truth, computed WITHOUT the predicate: scan every row and read the
/// field off it. Any tier that claims to serve the predicate has to reproduce
/// this exactly — a shorter answer is a silent false negative.
fn scanned(storage: &ColumnarStorage, field: &str, accept: impl Fn(&str) -> bool) -> Vec<String> {
    let mut names: Vec<String> = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("unfiltered scan")
        .into_iter()
        .filter(|m| m.fields.get(field).is_some_and(|v| accept(v)))
        .map(|m| m.name)
        .collect();
    names.sort();
    names
}

/// Build a segment from `table` at `dir` and open it — the shape the dirty
/// overlay holds for a file edited this session.
fn flush_segment(table: &SymbolTable, content_id: &[u8], dir: &Path) -> SegmentReader {
    let mut builder = SegmentBuilder::new("test", content_id);
    for row in &table.rows {
        let row_id = builder.emit_row(SymbolRow {
            name: table.name_of(row),
            fql_kind: table.fql_kind_of(row),
            language: table.language_of(row),
            line: u32::try_from(row.line).unwrap_or(u32::MAX),
            byte_start: u32::try_from(row.byte_range.start).unwrap_or(u32::MAX),
            byte_end: u32::try_from(row.byte_range.end).unwrap_or(u32::MAX),
            usages_count: row.usages_count,
        });
        if let Some(ordinal) = row.ordinal {
            builder.set_ordinal(row_id, ordinal);
        }
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder.flush(dir).expect("segment flush");
    SegmentReader::open(dir).expect("SegmentReader::open")
}

#[test]
fn eq_on_a_partly_posted_field_keeps_every_segment() {
    // `guard_branch` is posted by the two-arm file and not by the ten-arm one,
    // so the overlay's `guard_branch=0` bitmap names only the two-arm row.
    // A candidate set that is just that bitmap has already lost `arm_0`.
    let (_tmp, storage) = guard_workspace();

    let expected = scanned(&storage, "guard_branch", |v| v == "0");
    assert!(
        expected.iter().any(|n| n == "arm_0") && expected.iter().any(|n| n == "two_a"),
        "fixture must put a guard_branch = 0 row in BOTH files; got {expected:?}"
    );

    assert_eq!(
        found(&storage, "guard_branch", CompareOp::Eq, "0"),
        expected,
        "a segment that posts no values for the field must stay a candidate"
    );
}

#[test]
fn regex_on_a_posted_field_matches_the_scan() {
    let (_tmp, storage) = guard_workspace();

    let expected = scanned(&storage, "guard_kind", |v| v.starts_with("prep"));
    assert!(
        !expected.is_empty(),
        "fixture must produce preprocessor-guarded rows"
    );
    assert_eq!(
        found(&storage, "guard_kind", CompareOp::Matches, "^prep"),
        expected
    );
}

#[test]
fn like_on_a_posted_field_matches_the_scan() {
    let (_tmp, storage) = guard_workspace();

    let expected = scanned(&storage, "guard_branch", |v| v.starts_with('1'));
    assert!(!expected.is_empty(), "fixture must produce a branch 1 row");
    assert_eq!(
        found(&storage, "guard_branch", CompareOp::Like, "1%"),
        expected
    );
}

#[test]
fn negated_regex_on_a_posted_field_matches_the_scan() {
    let (_tmp, storage) = guard_workspace();

    // Only rows that CARRY the field and fail the pattern match, so a
    // candidate set built by subtraction still has to let the residual filter
    // drop every row that has no value for the field at all.
    let expected = scanned(&storage, "guard_branch", |v| !v.starts_with('0'));
    assert!(
        !expected.is_empty(),
        "fixture must produce rows on a branch other than 0"
    );
    assert_eq!(
        found(&storage, "guard_branch", CompareOp::NotMatches, "^0"),
        expected
    );
}

#[test]
fn a_pattern_that_accepts_the_empty_value_matches_the_scan() {
    let (_tmp, storage) = guard_workspace();

    // The overlay never keys an empty value, so for a pattern that accepts one
    // the value universe is not a complete account of what can match and the
    // query has to fall back to the scan rather than answer from the universe.
    let expected = scanned(&storage, "guard_branch", |_| true);
    assert!(!expected.is_empty(), "fixture must carry guard_branch rows");
    assert_eq!(
        found(&storage, "guard_branch", CompareOp::Matches, ".*"),
        expected
    );
}

/// Ground truth for a `name` predicate, computed the same way: scan, then read
/// the name off each row.
fn scanned_names(storage: &ColumnarStorage, accept: impl Fn(&str) -> bool) -> Vec<String> {
    let mut names: Vec<String> = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("unfiltered scan")
        .into_iter()
        .filter(|m| accept(&m.name))
        .map(|m| m.name)
        .collect();
    names.sort();
    names
}

#[test]
fn name_regex_with_alternation_matches_the_scan() {
    let (_tmp, storage) = guard_workspace();

    // Alternation is the shape the literal-trigram prefilter cannot serve: a
    // match needs the literals of ONE branch, so intersecting the branches'
    // candidate sets drops every real match. It bails to a scan instead —
    // correct, and the case the value-universe tier exists to make fast.
    let expected = scanned_names(&storage, |n| n == "arm_1" || n == "two_b");
    assert_eq!(
        expected,
        vec!["arm_1".to_owned(), "two_b".to_owned()],
        "fixture must carry exactly these two names"
    );
    assert_eq!(
        found(&storage, "name", CompareOp::Matches, "^(arm_1|two_b)$"),
        expected
    );
}

#[test]
fn a_name_regex_that_accepts_the_empty_string_matches_the_scan() {
    let (_tmp, storage) = guard_workspace();

    // A row with no name is not a key in the name FST, so a pattern that
    // accepts the empty string cannot be answered from the FST's key set.
    let expected = scanned_names(&storage, |_| true);
    assert!(!expected.is_empty(), "fixture must carry rows");
    assert_eq!(found(&storage, "name", CompareOp::Matches, ".*"), expected);
}

#[test]
fn an_uncommitted_row_is_found_by_a_pattern_on_a_posted_field() {
    let (tmp, mut storage) = guard_workspace();

    // A file indexed this session but never merged into the overlay. Its rows
    // reach the answer through the dirty stage, downstream of the candidate
    // bitmap — so a tier that narrows the bitmap must not be able to hide it.
    let abs = tmp.path().join("work").join("c_new.cpp");
    fs::write(&abs, "#if defined(OPT_NEW)\nint dirty_arm;\n#endif\n").expect("write c_new.cpp");
    let table = index_at_path(&CppLanguage, &abs);
    let staging = tmp.path().join("staging").join("c_new");
    let reader = flush_segment(&table, &[0x77u8; 8], &staging);
    storage.dirty_mut().add_segment(
        Arc::new(reader),
        PathBuf::from("c_new.cpp"),
        String::new(), // replaces nothing: a new file, not an edit
    );

    let got = found(&storage, "guard_kind", CompareOp::Matches, "^prep");
    assert!(
        got.iter().any(|n| n == "dirty_arm"),
        "an uncommitted row must be found by a regex on a posted field; got {got:?}"
    );
}

#[test]
fn a_field_keyed_by_row_walk_needs_no_backfill_and_still_matches_the_scan() {
    // Two ways a field gets keys, and only one can be partial. A field on the
    // segment posting list is keyed FROM postings, so a file over the posting
    // budget leaves a hole the candidate set has to fill. Every other field is
    // keyed by walking each segment's rows, so its key set is already whole —
    // and since no segment posts such a field, filling it "just in case" would
    // hand back every row of every file that carries the column.
    let (_tmp, storage) = workspace(&[(
        "a_numbers.cpp",
        "int a = 0x2A;\nint b = 0x2A;\nint c = 41;\n".to_owned(),
    )]);

    let expected = scanned(&storage, "num_format", |v| v == "hex");
    assert!(
        !expected.is_empty(),
        "fixture must carry hex literals; got none"
    );
    assert_eq!(
        found(&storage, "num_format", CompareOp::Eq, "hex"),
        expected
    );
    assert_eq!(
        found(&storage, "num_format", CompareOp::Matches, "^he"),
        expected
    );
}
