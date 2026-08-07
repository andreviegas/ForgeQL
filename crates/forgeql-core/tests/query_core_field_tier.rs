//! `WHERE language = '<lang>'` is answered from the stored language column.
//!
//! `language` is a core row column rather than an enrichment posting, so no
//! index tier served it: the query materialised every row in the corpus — full
//! result row and enrichment map apiece — so the residual filter could read a
//! field the row already carried, at 4.6 s per query on a 3M-symbol corpus.
//!
//! Reading the column instead makes the candidate set EXACT rather than a
//! superset, which is a stronger claim than any posting-derived tier can make,
//! and these tests hold it to that: every operator's answer must equal what an
//! unfiltered scan would have kept, an absent language must answer empty, and
//! a row whose language is empty must match nothing at all — not even a
//! negation, because the row-level filter fails every operator on a field it
//! reports as absent.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

mod overlay_harness;

use std::fs;
use std::path::Path;

use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::ColumnarStorage;
use forgeql_core::storage::columnar::overlay::Overlay;
use overlay_harness::*;

/// A real multi-segment, multi-LANGUAGE workspace. Two languages matter here:
/// with one, every candidate set is trivially the whole corpus and a tier that
/// ignored the column entirely would still pass.
fn mixed_language_workspace() -> (TempDir, ColumnarStorage) {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().join("work");
    fs::create_dir_all(&root).expect("create worktree root");
    let segments_dir = tmp.path().join("segments");

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();

    let cpp = root.join("a_two.cpp");
    fs::write(&cpp, "int cpp_one(int x) { return x + 1; }\nint cpp_two;\n").expect("write cpp");
    let _ = segment_map.insert(
        cpp.clone(),
        build_segment(&index_at_path(&CppLanguage, &cpp), &cpp, &segments_dir),
    );

    let rs = root.join("b_two.rs");
    fs::write(&rs, "pub fn rust_one() -> u32 { 1 }\npub struct RustTwo;\n").expect("write rs");
    let _ = segment_map.insert(
        rs.clone(),
        build_segment(&index_at_path(&RustLanguage, &rs), &rs, &segments_dir),
    );

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
        ColumnarStorage::new(root, segs, overlay, Arc::new(LanguageRegistry::new(vec![])));
    (tmp, storage)
}

/// Sorted `(name, language)` pairs returned for one WHERE predicate.
fn found(storage: &ColumnarStorage, op: CompareOp, value: &str) -> Vec<(String, String)> {
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "language".to_owned(),
            op,
            value: PredicateValue::String(value.to_owned()),
        }],
        ..Clauses::default()
    };
    let mut rows: Vec<(String, String)> = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("find_symbols")
        .into_iter()
        .map(|m| (m.name, m.language.unwrap_or_default()))
        .collect();
    rows.sort();
    rows
}

/// Ground truth, computed WITHOUT the predicate: scan every row and read the
/// language off it. A row reporting no language can satisfy no operator.
fn scanned(storage: &ColumnarStorage, accept: impl Fn(&str) -> bool) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("unfiltered scan")
        .into_iter()
        .filter(|m| m.language.as_deref().is_some_and(&accept))
        .map(|m| (m.name, m.language.unwrap_or_default()))
        .collect();
    rows.sort();
    rows
}

#[test]
fn eq_on_language_matches_the_scan() {
    let (_tmp, storage) = mixed_language_workspace();

    let expected = scanned(&storage, |v| v == "cpp");
    assert!(!expected.is_empty(), "fixture must produce cpp rows");
    assert!(
        expected.len() < scanned(&storage, |_| true).len(),
        "fixture must also produce rows of another language, or this proves nothing"
    );
    assert_eq!(found(&storage, CompareOp::Eq, "cpp"), expected);
}

#[test]
fn an_absent_language_answers_empty() {
    let (_tmp, storage) = mixed_language_workspace();

    // The column is read for every row of every segment, so an empty answer
    // here is a claim the tier is entitled to make rather than a scan it
    // declined to run.
    assert!(
        found(&storage, CompareOp::Eq, "cobol").is_empty(),
        "a language the corpus does not contain must answer empty"
    );
    assert!(
        scanned(&storage, |v| v == "cobol").is_empty(),
        "and the scan must agree it is genuinely absent"
    );
}

#[test]
fn regex_and_like_on_language_match_the_scan() {
    let (_tmp, storage) = mixed_language_workspace();

    let cpp = scanned(&storage, |v| v == "cpp");
    assert_eq!(found(&storage, CompareOp::Matches, "^cpp$"), cpp);
    assert_eq!(found(&storage, CompareOp::Like, "cp%"), cpp);
}

#[test]
fn negated_operators_on_language_match_the_scan() {
    let (_tmp, storage) = mixed_language_workspace();

    let not_cpp = scanned(&storage, |v| v != "cpp");
    assert!(
        !not_cpp.is_empty(),
        "fixture must produce rows of a language other than cpp"
    );
    assert_eq!(found(&storage, CompareOp::NotEq, "cpp"), not_cpp);
    assert_eq!(found(&storage, CompareOp::NotMatches, "^cpp$"), not_cpp);

    // A row that reports no language at all satisfies neither form. The scan
    // is the authority on that, and both answers are compared to it.
    let all = scanned(&storage, |_| true);
    let cpp = scanned(&storage, |v| v == "cpp");
    assert_eq!(
        cpp.len() + not_cpp.len(),
        all.len(),
        "every row WITH a language falls in exactly one of the two answers"
    );
}

#[test]
fn a_path_scoped_language_query_answers_what_the_scan_keeps() {
    let (_tmp, storage) = mixed_language_workspace();

    // A path filter with no *indexed* predicate seeds every row of every
    // matching segment and skips the global prefilter entirely, so `language`
    // has to count as indexed or the shape an agent writes most —
    // `IN '<dir>/**' WHERE language = '<lang>'` — silently keeps the old cost.
    //
    // What this pins is the ANSWER under a path filter, not which branch
    // produced it. `**` yields no path floor (the glob has no literal prefix
    // ending in `/`), and the floor-bearing form needs a fixture in a
    // subdirectory, which the shared segment helper cannot express — it stores
    // every fixture under its bare file name. The `core_eq` A/B is what holds
    // the routing honest; this holds the result honest either way.
    let clauses = Clauses {
        in_glob: Some("**".to_owned()),
        where_predicates: vec![Predicate {
            field: "language".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("cpp".to_owned()),
        }],
        ..Clauses::default()
    };
    let mut got: Vec<(String, String)> = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("find_symbols")
        .into_iter()
        .map(|m| (m.name, m.language.unwrap_or_default()))
        .collect();
    got.sort();

    assert_eq!(
        got,
        scanned(&storage, |v| v == "cpp"),
        "a path-scoped language query must return exactly what the scan keeps"
    );
    // Which branch actually ran is a cost property, and cost is not something
    // this suite can observe — both branches answer completely. The `core_eq`
    // A/B is what holds the routing honest.
}

#[test]
fn an_uncommitted_row_is_found_by_a_language_predicate() {
    let (tmp, mut storage) = mixed_language_workspace();

    // Dirty rows are materialised in their own stage, downstream of the
    // candidate bitmap, so a tier that narrows the bitmap must not hide one.
    let abs = tmp.path().join("work").join("c_new.cpp");
    fs::write(&abs, "int dirty_cpp_symbol;\n").expect("write c_new.cpp");
    let table = index_at_path(&CppLanguage, &abs);
    let staging = tmp.path().join("staging").join("c_new");
    let mut builder = SegmentBuilder::new("test", &[0x77u8; 8]);
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
    builder.flush(&staging).expect("segment flush");
    let reader = SegmentReader::open(&staging).expect("SegmentReader::open");
    storage.dirty_mut().add_segment(
        Arc::new(reader),
        PathBuf::from("c_new.cpp"),
        String::new(), // replaces nothing: a new file, not an edit
    );

    let got = found(&storage, CompareOp::Eq, "cpp");
    assert!(
        got.iter().any(|(name, _)| name == "dirty_cpp_symbol"),
        "an uncommitted row must be found by a language predicate; got {got:?}"
    );
}
