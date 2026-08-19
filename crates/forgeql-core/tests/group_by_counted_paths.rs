//! The two older counted groupings — `GROUP BY fql_kind` and `GROUP BY file` —
//! held to the equality the enrichment one is held to.
//!
//! Both answer from stored structures instead of building rows, so both can
//! report numbers no row was ever consulted for. Each case here runs one query
//! twice over one overlay, counted and through the scan, and compares the group
//! maps: a `WHERE line >= 0` holds for every row, so it changes no group and no
//! count, and it is what disarms a count path.
//!
//! Each case also asserts WHICH route answered. No golden can see that, and two
//! routes that have quietly become one route pass every comparison here.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

mod overlay_harness;

use std::collections::BTreeMap;
use std::path::Path;

use overlay_harness::*;

use forgeql_core::ir::{CompareOp, GroupBy, Predicate, PredicateValue};
use forgeql_core::storage::StorageEngine;

/// Group key → count for a `GROUP BY fql_kind` page. A row whose kind does not
/// resolve belongs to the empty group, which is how `apply_group_by` keys it on
/// the scan and the only way the counted route can report it at all.
fn kind_groups(page: &[SymbolMatch]) -> BTreeMap<String, usize> {
    page.iter()
        .map(|r| {
            (
                r.fql_kind.clone().unwrap_or_default(),
                r.count.expect("a GROUP BY row carries its count"),
            )
        })
        .collect()
}

/// The same for a `GROUP BY file` page.
fn file_groups(page: &[SymbolMatch]) -> BTreeMap<String, usize> {
    page.iter()
        .map(|r| {
            (
                r.path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                r.count.expect("a GROUP BY row carries its count"),
            )
        })
        .collect()
}

fn group_by(field: &str) -> Clauses {
    Clauses {
        group_by: Some(GroupBy::Field(field.to_owned())),
        ..Clauses::default()
    }
}

/// The same clauses forced through the scan: `line >= 0` holds for every row so
/// it removes nothing, and a `WHERE` is what disarms a count path.
fn via_scan(clauses: &Clauses) -> Clauses {
    let mut scanned = clauses.clone();
    scanned.where_predicates.push(Predicate {
        field: "line".to_owned(),
        op: CompareOp::Gte,
        value: PredicateValue::Number(0),
    });
    scanned
}

fn predicate(field: &str, value: &str) -> Predicate {
    Predicate {
        field: field.to_owned(),
        op: CompareOp::Eq,
        value: PredicateValue::String(value.to_owned()),
    }
}

/// A `GROUP BY fql_kind` row counted from the index carries no path, because no
/// row of the group was ever opened; the scan's carries the first matching row's.
fn kind_group_came_from_the_index(page: &[SymbolMatch]) -> bool {
    !page.is_empty() && page.iter().all(|r| r.path.is_none())
}

/// Both routes give a `GROUP BY file` row its path, so the path cannot tell them
/// apart. The name can: counted, there is no row to take one from.
fn file_group_came_from_the_index(page: &[SymbolMatch]) -> bool {
    !page.is_empty() && page.iter().all(|r| r.name.is_empty() && r.line.is_none())
}

/// Lowercase hex of a content id, the spelling `seg_path` expects.
fn hex_of(content_id: &[u8]) -> String {
    content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;

        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// An overlay over two segments: `canonical.cpp`, indexed normally — and a
/// synthetic one carrying the two shapes the counted paths get wrong.
///
/// Three of its rows have no `fql_kind` at all. `step5_build_kind_postings`
/// skips the empty kind, so no kind bitmap accounts for them and only the
/// subtraction against the canonical total can put them in a group.
///
/// Two more are the same row twice. The dedupe pass collapses them into the one
/// row the answer holds; `step6_build_name_fst` does not intersect its postings
/// with the canonical set, so the name index still proposes both — which is why
/// a `name =` beside a counted `GROUP BY file` reported one row too many, and
/// why it is no longer counted.
fn overlay_with_kindless_and_duplicate_rows()
-> (TempDir, forgeql_core::storage::columnar::ColumnarStorage) {
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let table = index_fixture(&CppLanguage, "canonical.cpp");
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let cpp_id = build_segment(&table, &cpp_path, &segments_dir);

    let synth_path = fixture_path("canonical.rs");
    let synth_id: Vec<u8> = vec![0xb1, 0xc2, 0xd3, 0xe4, 0xf5, 0x06, 0x17, 0x28];
    let mut builder = SegmentBuilder::new("test", &synth_id);
    for i in 0..3u32 {
        let name = format!("kindless_{i}");
        let _ = builder.emit_row(SymbolRow {
            name: &name,
            fql_kind: "",
            language: "rust",
            line: i + 1,
            byte_start: i,
            byte_end: i + 1,
            usages_count: 0,
        });
    }
    for _ in 0..2 {
        let _ = builder.emit_row(SymbolRow {
            name: "twin",
            fql_kind: "function",
            language: "rust",
            line: 10,
            byte_start: 100,
            byte_end: 110,
            usages_count: 0,
        });
    }
    builder
        .flush(&seg_path(
            &segments_dir,
            Path::new("canonical.rs"),
            &hex_of(&synth_id),
        ))
        .expect("segment flush");

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_id);
    let _ = segment_map.insert(synth_path, synth_id);

    let overlay_path = overlays_dir.join("test").join("counted_paths.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
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
    let storage = ColumnarStorage::new_unshared(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
    (tmp, storage)
}

/// The kind groups are the scan's kind groups, including the group the kind
/// index has no entry for.
#[test]
fn the_kind_groups_are_the_scans_kind_groups() {
    let (_tmp, storage) = overlay_with_kindless_and_duplicate_rows();

    let counted = storage
        .find_symbols(&group_by("fql_kind"), Path::new("."))
        .expect("counted route");
    let scanned = storage
        .find_symbols(&via_scan(&group_by("fql_kind")), Path::new("."))
        .expect("scan route");

    assert!(
        kind_group_came_from_the_index(&counted),
        "GROUP BY fql_kind was not counted from the index"
    );
    assert!(
        scanned.iter().any(|r| r.path.is_some()),
        "the control did not go through the scan"
    );
    assert_eq!(kind_groups(&counted), kind_groups(&scanned));
    assert_eq!(counted.total, scanned.total, "group count");

    // Without this the equality above holds vacuously the day the fixture stops
    // producing a row with no kind — the defect this fixes leaves no trace when
    // there is nothing for it to drop.
    assert_eq!(
        kind_groups(&counted).get("").copied(),
        Some(3),
        "the rows with no kind are one group of their own"
    );
}

/// And they add up to the whole answer, not to a part of it.
#[test]
fn the_kind_groups_partition_the_whole_answer() {
    let (_tmp, storage) = overlay_with_kindless_and_duplicate_rows();

    let counted = storage
        .find_symbols(&group_by("fql_kind"), Path::new("."))
        .expect("counted route");
    let all = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("every row");

    let summed: usize = counted.iter().filter_map(|r| r.count).sum();
    assert_eq!(summed, all.rows.len());
}

/// A `name =` beside a counted `GROUP BY file` goes to the scan, and the count
/// it comes back with is the number of rows the answer holds.
#[test]
fn a_name_predicate_leaves_the_counted_route() {
    let (_tmp, storage) = overlay_with_kindless_and_duplicate_rows();

    let clauses = Clauses {
        where_predicates: vec![predicate("name", "twin")],
        ..group_by("file")
    };
    let page = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("name = beside GROUP BY file");
    let scanned = storage
        .find_symbols(&via_scan(&clauses), Path::new("."))
        .expect("scan route");

    assert!(
        !file_group_came_from_the_index(&page),
        "a name predicate must not be counted: no name tier verifies its candidates"
    );
    assert_eq!(file_groups(&page), file_groups(&scanned));

    // The number is the whole point: the name postings carry both raw rows, so
    // counting them reports two where the answer holds one.
    let summed: usize = page.iter().filter_map(|r| r.count).sum();
    assert_eq!(summed, 1, "the duplicate row is one row of the answer");
}

/// The kind predicate is still counted — the fix is not to decline everything.
#[test]
fn a_kind_predicate_stays_on_the_counted_route() {
    let (_tmp, storage) = overlay_with_kindless_and_duplicate_rows();

    let clauses = Clauses {
        where_predicates: vec![predicate("fql_kind", "function")],
        ..group_by("file")
    };
    let page = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("fql_kind = beside GROUP BY file");
    let scanned = storage
        .find_symbols(&via_scan(&clauses), Path::new("."))
        .expect("scan route");

    assert!(
        file_group_came_from_the_index(&page),
        "fql_kind = is the exact tier and must still be counted"
    );
    assert_eq!(file_groups(&page), file_groups(&scanned));
}

/// A `HAVING` naming a field the group row does not carry sends the query to the
/// scan rather than being evaluated against rows that cannot answer it.
#[test]
fn a_having_the_group_row_cannot_answer_reaches_the_scan() {
    let (_tmp, storage) = overlay_with_kindless_and_duplicate_rows();

    for grouped in ["fql_kind", "file"] {
        let clauses = Clauses {
            having_predicates: vec![Predicate {
                field: "lines".to_owned(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(2),
            }],
            ..group_by(grouped)
        };
        let page = storage
            .find_symbols(&clauses, Path::new("."))
            .expect("HAVING on a row field");
        let scanned = storage
            .find_symbols(&via_scan(&clauses), Path::new("."))
            .expect("scan route");

        // An empty set here is a claim about the corpus, and it was the wrong
        // one: the predicate was false on every group because no group row
        // carried the field it names.
        assert!(
            !page.rows.is_empty(),
            "GROUP BY {grouped} HAVING lines >= 2 came back empty"
        );
        assert_eq!(
            page.rows.len(),
            scanned.rows.len(),
            "GROUP BY {grouped} HAVING lines >= 2"
        );
    }
}
