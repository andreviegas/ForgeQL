//! The set-valued guard fields are indexed by their WHOLE value.
//!
//! `guard_defines`, `guard_mentions` and `guard_negates` hold a comma-joined
//! set, and `filter::eval_predicate` compares `=` against the whole joined
//! string. So the posting index keys the whole value too — the thing the query
//! layer actually compares. Keying the individual members instead would key
//! something nothing compares against, and two tiers that read these postings
//! as values would break silently: the pattern tier would union nothing for a
//! regex spanning a comma, and the absence proof would call a whole value
//! absent because it is not a member. Both are pinned below.
//!
//! Membership needs no operator of its own — `MATCHES '(^|,)X(,|$)'` is exact,
//! and the same postings serve it through the value-universe path.

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
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{
    ColumnarStorage, POSTING_ENRICHMENT_FIELDS, overlay_budget, posting_budget,
};
use overlay_harness::*;

/// Every field the guard enricher joins with commas, plus the two other wide
/// fields this tier serves.
const WIDE: &[&str] = &[
    "guard_defines",
    "guard_mentions",
    "guard_negates",
    "guard_group_id",
    "key_path",
];

/// A workspace of real segments; `find_symbols` here runs the production
/// columnar pipeline.
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
/// field off it. Any tier claiming to serve the predicate must reproduce this.
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

/// One guard arm per distinct define, so `guard_defines` takes `count`
/// distinct values in this one file.
fn many_distinct_defines(count: usize) -> String {
    let mut src = String::new();
    for i in 0..count {
        let _ = write!(src, "#if defined(FLAG_{i})\nint wide_{i};\n#endif\n");
    }
    src
}

/// Two rows whose `guard_defines` is a genuine multi-member set, plus one
/// single-member row that shares a member with them.
const SETS: &str = "#if defined(ALPHA) && defined(BETA)\nint both_ab;\n#endif\n\
                    #if defined(ALPHA)\nint only_a;\n#endif\n\
                    #if !defined(GAMMA) && !defined(DELTA)\nint neither_gd;\n#endif\n";

#[test]
fn a_wide_budget_on_a_field_nothing_posts_would_be_dead_config() {
    // The two halves have to agree or the tier is silently absent: a field can
    // carry a wide budget and never be posted, and the only symptom is that
    // every query on it stays a full scan.
    for field in WIDE {
        assert!(
            POSTING_ENRICHMENT_FIELDS.contains(field),
            "{field} has a wide budget but nothing posts it"
        );
        assert!(
            posting_budget(field) > posting_budget("naming"),
            "{field} must not share the handful-of-values budget"
        );
        assert!(
            overlay_budget(field) > overlay_budget("naming"),
            "{field} must not share the handful-of-values overlay budget"
        );
    }
}

#[test]
fn the_set_fields_are_actually_posted() {
    // The failure this pins is config that reads correctly and indexes nothing.
    let tmp = TempDir::new().expect("tempdir");
    let abs = tmp.path().join("guards.cpp");
    fs::write(&abs, SETS).expect("write guards.cpp");
    let table = index_at_path(&CppLanguage, &abs);
    let seg_dir = tmp.path().join("seg");
    let mut builder = SegmentBuilder::new("test", &[0x11u8; 8]);
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
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder.flush(&seg_dir).expect("segment flush");
    let reader = SegmentReader::open(&seg_dir).expect("SegmentReader::open");

    for field in [
        "guard_defines",
        "guard_negates",
        "guard_mentions",
        "guard_group_id",
    ] {
        assert!(
            reader.has_extra_col(field),
            "fixture must produce a {field} column"
        );
        assert!(
            reader.posts_field(field),
            "{field} is stored but not indexed — the tier is dead"
        );
    }
}

#[test]
fn eq_on_a_set_field_is_the_whole_value_not_membership() {
    // The decision this slice rests on. `both_ab` has guard_defines
    // "ALPHA,BETA"; asking for `ALPHA` must not return it, because that is not
    // what `=` has ever meant here. If the postings were keyed by member, the
    // candidate set would offer it and only the residual filter would catch it
    // — and for the pattern tier there is no residual rescue.
    let (_tmp, storage) = workspace(&[("a_sets.cpp", SETS.to_owned())]);

    let whole = scanned(&storage, "guard_defines", |v| v == "ALPHA,BETA");
    assert!(
        whole.iter().any(|n| n == "both_ab"),
        "fixture must produce a multi-member set; got {whole:?}"
    );
    assert_eq!(
        found(&storage, "guard_defines", CompareOp::Eq, "ALPHA,BETA"),
        whole
    );

    let single = found(&storage, "guard_defines", CompareOp::Eq, "ALPHA");
    assert!(
        !single.contains(&"both_ab".to_owned()),
        "= 'ALPHA' must not match the row whose value is 'ALPHA,BETA'; got {single:?}"
    );
    assert_eq!(single, scanned(&storage, "guard_defines", |v| v == "ALPHA"));
}

#[test]
fn the_membership_recipe_matches_the_scan() {
    // Membership without a new operator, served by the same postings through
    // the value-universe path: the regex is tested against each distinct
    // VALUE, and a value spanning a comma is still one value.
    let (_tmp, storage) = workspace(&[("a_sets.cpp", SETS.to_owned())]);

    let contains_alpha = scanned(&storage, "guard_defines", |v| {
        v.split(',').any(|m| m == "ALPHA")
    });
    assert!(
        contains_alpha.len() >= 2,
        "fixture must have both a single-member and a multi-member ALPHA row; got {contains_alpha:?}"
    );
    assert_eq!(
        found(
            &storage,
            "guard_defines",
            CompareOp::Matches,
            "(^|,)ALPHA(,|$)"
        ),
        contains_alpha
    );

    // And the comma-spanning regex a member-keyed index would have lost.
    assert_eq!(
        found(&storage, "guard_defines", CompareOp::Matches, "ALPHA,BETA"),
        scanned(&storage, "guard_defines", |v| v.contains("ALPHA,BETA"))
    );
}

#[test]
fn a_file_over_its_field_budget_still_answers_completely() {
    // A file whose distinct-value count for a wide field exceeds the per-file
    // budget writes no postings blob for it — never a partial one. Its rows
    // still carry the column, so they have to stay candidates; a candidate set
    // narrowed to the other file's bitmap would lose them silently.
    let over = posting_budget("guard_defines") + 1;
    let (_tmp, storage) = workspace(&[
        ("a_wide.cpp", many_distinct_defines(over)),
        (
            "b_narrow.cpp",
            "#if defined(FLAG_0)\nint narrow_hit;\n#endif\n".to_owned(),
        ),
    ]);

    let expected = scanned(&storage, "guard_defines", |v| v == "FLAG_0");
    assert!(
        expected.iter().any(|n| n == "wide_0") && expected.iter().any(|n| n == "narrow_hit"),
        "fixture must put a FLAG_0 row in BOTH files; got {expected:?}"
    );
    assert_eq!(
        found(&storage, "guard_defines", CompareOp::Eq, "FLAG_0"),
        expected,
        "an over-budget file must stay a candidate rather than vanish"
    );

    // The same has to hold for the pattern tier, which unions value bitmaps.
    assert_eq!(
        found(
            &storage,
            "guard_defines",
            CompareOp::Matches,
            "(^|,)FLAG_0(,|$)"
        ),
        expected
    );

    // The NUMERIC arms need the same compensation, and do now get it — but
    // this suite cannot demonstrate it through a query, for a reason worth
    // writing down. `guard_group_id` is the only posted field parsed as a
    // number, and its values are u64 hashes that routinely exceed i64::MAX.
    // `PredicateValue::Number` and `ClauseTarget::field_num` are both i64, so
    // the row's own value fails to parse and no numeric comparison on
    // `guard_group_id` can match it — a pre-existing limit of the numeric
    // predicate, not of this index. Attempting the assertion here panics with
    // PosOverflow on the fixture's real ids.
    //
    // The compensation is applied anyway (`prefilter_global`'s four
    // `PredicateValue::Number` arms union `rows_missing_field_postings`, as
    // the string arms do) because it costs nothing — that helper returns an
    // empty bitmap for every field that is not posted — and because leaving
    // one arm of three uncompensated is how the gap this whole slice is about
    // gets reintroduced.
}

#[test]
fn an_uncommitted_row_is_found_by_a_set_field_query() {
    // Session-born rows reach the answer in their own stage, downstream of the
    // candidate bitmap; a tier that narrows the bitmap must not hide one.
    //
    // OMEGA appears in no committed file, so the overlay holds no key for it.
    // That is the case posting these fields newly made reachable: the absence
    // proof could not conclude anything about them before (no postings blob
    // meant no proof), and now it can answer an empty bitmap. It must not do so
    // while an uncommitted row carries the value.
    let (tmp, mut storage) = workspace(&[("a_sets.cpp", SETS.to_owned())]);

    let abs = tmp.path().join("work").join("c_new.cpp");
    fs::write(&abs, "#if defined(OMEGA)\nint dirty_omega;\n#endif\n").expect("write c_new.cpp");
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
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder.flush(&staging).expect("segment flush");
    storage.dirty_mut().add_segment(
        Arc::new(SegmentReader::open(&staging).expect("SegmentReader::open")),
        PathBuf::from("c_new.cpp"),
        String::new(), // replaces nothing: a new file, not an edit
    );

    let got = found(&storage, "guard_defines", CompareOp::Eq, "OMEGA");
    assert!(
        got.iter().any(|n| n == "dirty_omega"),
        "a value carried only by an uncommitted row must not be answered absent; got {got:?}"
    );
}
