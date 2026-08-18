//! `GROUP BY <enrichment field>` counted from the overlay's key table.
//!
//! The whole claim of that path is an equality: the stored per-value bitmap
//! cardinalities are the counts the scan's grouping pass produces, including
//! the group of rows the field says nothing about. These cases hold it to that
//! by running both routes over one overlay and comparing the group maps. A
//! `WHERE` every row satisfies changes no group and no count, and is the one
//! thing that disarms the count path, so it is what the scan's answer is asked
//! for with.

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

/// Fields the segment builder posts per value AND `canonical.cpp` carries, so
/// the count path accepts them and there is something to count.
const POSTED: &[&str] = &["naming", "has_doc", "scope", "comment_style"];

/// Group key → count, read the way the renderer reads a group row: a row that
/// resolves the grouped field belongs to that value's group, one that does not
/// belongs to the empty group. Both routes are read the same way, which is
/// what makes the comparison meaningful.
fn groups(page: &[SymbolMatch], field: &str) -> BTreeMap<String, usize> {
    page.iter()
        .map(|r| {
            (
                r.fields.get(field).cloned().unwrap_or_default(),
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

/// The same grouping, forced through the scan: `line >= 0` holds for every row
/// so it removes nothing, and a `WHERE` is what disarms the count path.
fn group_by_via_scan(field: &str) -> Clauses {
    Clauses {
        where_predicates: vec![Predicate {
            field: "line".to_owned(),
            op: CompareOp::Gte,
            value: PredicateValue::Number(0),
        }],
        ..group_by(field)
    }
}

/// A group row counted from the index carries no path, because no row of the
/// group was ever opened; the scan's carries the first matching row's. That is
/// what tells the two routes apart from outside the engine — without it a case
/// comparing the routes passes just as happily when both took the same one.
fn came_from_the_index(page: &[SymbolMatch]) -> bool {
    !page.is_empty() && page.iter().all(|r| r.path.is_none())
}

#[test]
fn counted_groups_are_the_scans_groups() {
    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    for field in POSTED {
        let counted = storage
            .find_symbols(&group_by(field), Path::new("."))
            .expect("counted route");
        let scanned = storage
            .find_symbols(&group_by_via_scan(field), Path::new("."))
            .expect("scan route");

        assert!(
            came_from_the_index(&counted),
            "GROUP BY {field} was not counted from the index"
        );
        assert!(
            scanned.iter().any(|r| r.path.is_some()),
            "the control for {field} did not go through the scan"
        );
        assert_eq!(
            groups(&counted, field),
            groups(&scanned, field),
            "GROUP BY {field}"
        );
        assert_eq!(counted.total, scanned.total, "GROUP BY {field} total");
    }
}

#[test]
fn the_groups_partition_the_whole_answer() {
    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let all = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("every row");

    for field in POSTED {
        let counted = storage
            .find_symbols(&group_by(field), Path::new("."))
            .expect("counted route");
        assert!(came_from_the_index(&counted), "GROUP BY {field}");
        let summed: usize = counted.iter().filter_map(|r| r.count).sum();
        assert_eq!(
            summed,
            all.rows.len(),
            "GROUP BY {field}: the groups must add up to the answer, not to a part of it"
        );
    }
}

/// Lowercase hex of a content id, the spelling `seg_path` expects.
fn hex_of(content_id: &[u8]) -> String {
    content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;

        let _ = write!(acc, "{b:02x}");
        acc
    })
}

/// An overlay over two segments: `canonical.cpp`, indexed normally, which posts
/// `naming` per value — and a second segment whose `naming` column holds more
/// distinct values than the per-field posting budget allows, so the builder
/// writes the column and skips the postings for it.
///
/// That second shape is what the count path has to refuse. Its rows carry
/// values the key table holds no key for, so counting from keys alone would not
/// merely miss them: it would find them missing from every value's bitmap and
/// move all nine into the group of rows that have no value at all.
fn overlay_with_a_partially_posted_segment()
-> (TempDir, forgeql_core::storage::columnar::ColumnarStorage) {
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let table = index_fixture(&CppLanguage, "canonical.cpp");
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let cpp_id = build_segment(&table, &cpp_path, &segments_dir);

    // Nine distinct values of a field whose narrow posting budget is eight.
    let wide_path = fixture_path("canonical.rs");
    let wide_id: Vec<u8> = vec![0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18];
    let mut builder = SegmentBuilder::new("test", &wide_id);
    for i in 0..9u32 {
        let name = format!("unposted_{i}");
        let row = builder.emit_row(SymbolRow {
            name: &name,
            fql_kind: "function",
            language: "rust",
            line: i + 1,
            byte_start: i,
            byte_end: i + 1,
            usages_count: 0,
        });
        builder.set_field(row, "naming", format!("style_{i}").as_str());
    }
    builder
        .flush(&seg_path(
            &segments_dir,
            Path::new("canonical.rs"),
            &hex_of(&wide_id),
        ))
        .expect("segment flush");

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_id);
    let _ = segment_map.insert(wide_path, wide_id);

    let overlay_path = overlays_dir.join("test").join("partially_posted.bin");
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

#[test]
fn a_segment_that_stores_the_column_without_posting_it_sends_the_group_to_the_scan() {
    let (_tmp, storage) = overlay_with_a_partially_posted_segment();

    let counted = storage
        .find_symbols(&group_by("naming"), Path::new("."))
        .expect("group by naming");
    let scanned = storage
        .find_symbols(&group_by_via_scan("naming"), Path::new("."))
        .expect("scan route");

    assert!(
        !came_from_the_index(&counted),
        "a segment storing `naming` without posting it must send GROUP BY naming to the scan"
    );
    assert_eq!(groups(&counted, "naming"), groups(&scanned, "naming"));

    // And the rows that make the refusal necessary are really there and really
    // carry a value: counted from the keys alone they would have arrived as
    // nine more rows with no value at all.
    let by_value = groups(&scanned, "naming");
    for i in 0..9 {
        assert_eq!(
            by_value.get(&format!("style_{i}")).copied(),
            Some(1),
            "row {i} of the unposted segment"
        );
    }
}
