use std::path::PathBuf;

use super::*;
use crate::ir::{Clauses, CompareOp, GroupBy, OrderBy, Predicate, PredicateValue, SortDirection};
use crate::storage::columnar::segment_builder::{SegmentBuilder, SymbolRow};

// ── helpers ──────────────────────────────────────────────────────────────

/// Write a segment with known rows to a temp dir and return the
/// (tempdir, segment path) pair.
fn make_segment(rows: &[(&str, &str, u32)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let content_id = [0xAB_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    for &(name, kind, line) in rows {
        b.add_row(SymbolRow {
            name,
            fql_kind: kind,
            language: "rust",
            line,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
    }
    b.flush(&seg).expect("flush");
    (tmp, seg)
}

fn clauses_where_kind(kind: &str) -> Clauses {
    Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String(kind.to_owned()),
        }],
        ..Clauses::default()
    }
}

fn names(results: &[SymbolMatch]) -> Vec<&str> {
    results.iter().map(|r| r.name.as_str()).collect()
}

// ── tests ─────────────────────────────────────────────────────────────────

#[test]
fn open_segment_written_by_builder() {
    let (_tmp, seg) = make_segment(&[("foo", "function", 1)]);
    let reader = SegmentReader::open(&seg).expect("open");
    assert_eq!(reader.row_count, 1);
    assert_eq!(reader.provider_id, "test");
}

#[test]
fn find_functions_order_by_name() {
    let (_tmp, seg) = make_segment(&[
        ("main", "function", 10),
        ("X_CONST", "variable", 5),
        ("helper", "function", 20),
    ]);
    let reader = SegmentReader::open(&seg).expect("open");

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("function".to_owned()),
        }],
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        ..Clauses::default()
    };

    let result = reader.find_symbols(&clauses, None).expect("find");
    assert_eq!(names(&result), ["helper", "main"]);
}

#[test]
fn find_by_enrichment_field() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let content_id = [0x11_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    let row = b.emit_row(SymbolRow {
        name: "foo",
        fql_kind: "function",
        language: "rust",
        line: 1,
        byte_start: 0,
        byte_end: 50,
        usages_count: 0,
    });
    b.set_field(row, "param_count", "2");
    let row2 = b.emit_row(SymbolRow {
        name: "bar",
        fql_kind: "function",
        language: "rust",
        line: 5,
        byte_start: 51,
        byte_end: 100,
        usages_count: 0,
    });
    b.set_field(row2, "param_count", "0");
    b.flush(&seg).expect("flush");

    let reader = SegmentReader::open(&seg).expect("open");

    // WHERE param_count = '2' should return only "foo"
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "param_count".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("2".to_owned()),
        }],
        ..Clauses::default()
    };
    let result = reader.find_symbols(&clauses, None).expect("find");
    assert_eq!(names(&result), ["foo"]);

    // The enrichment field should appear in the fields map.
    assert_eq!(result[0].fields.get("param_count"), Some(&"2".to_owned()));
}

#[test]
fn group_by_kind_having_count() {
    let (_tmp, seg) = make_segment(&[
        ("f1", "function", 1),
        ("f2", "function", 2),
        ("S1", "struct", 3),
    ]);
    let reader = SegmentReader::open(&seg).expect("open");

    let clauses = Clauses {
        group_by: Some(GroupBy::Field("fql_kind".to_owned())),
        having_predicates: vec![Predicate {
            field: "count".to_owned(),
            op: CompareOp::Gte,
            value: PredicateValue::Number(2),
        }],
        ..Clauses::default()
    };
    let result = reader.find_symbols(&clauses, None).expect("find");
    // Only "function" has count ≥ 2.
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].fql_kind.as_deref(), Some("function"));
    assert_eq!(result[0].count, Some(2));
}

#[test]
fn order_by_line_desc() {
    let (_tmp, seg) = make_segment(&[
        ("a", "function", 30),
        ("b", "function", 10),
        ("c", "function", 20),
    ]);
    let reader = SegmentReader::open(&seg).expect("open");

    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Desc,
        }),
        ..Clauses::default()
    };
    let result = reader.find_symbols(&clauses, None).expect("find");
    let lines: Vec<_> = result.iter().map(|r| r.line).collect();
    assert_eq!(lines, [Some(30), Some(20), Some(10)]);
}

#[test]
fn limit_and_offset() {
    let (_tmp, seg) = make_segment(&[
        ("r0", "function", 1),
        ("r1", "function", 2),
        ("r2", "function", 3),
        ("r3", "function", 4),
        ("r4", "function", 5),
    ]);
    let reader = SegmentReader::open(&seg).expect("open");

    // ORDER BY line ASC LIMIT 2 OFFSET 1 → rows at lines 2, 3
    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(2),
        offset: Some(1),
        ..Clauses::default()
    };
    let result = reader.find_symbols(&clauses, None).expect("find");
    assert_eq!(result.len(), 2);
    let lines: Vec<_> = result.iter().map(|r| r.line).collect();
    assert_eq!(lines, [Some(2), Some(3)]);
}

#[test]
fn lookup_name_via_fst() {
    let (_tmp, seg) = make_segment(&[
        ("foo", "function", 1),
        ("bar", "struct", 5),
        ("foo", "function", 10), // second row with same name
    ]);
    let reader = SegmentReader::open(&seg).expect("open");

    let rows = reader.lookup_name("foo");
    assert_eq!(rows.len(), 2, "two 'foo' rows");
    let mut rows_sorted = rows;
    rows_sorted.sort_unstable();
    assert_eq!(rows_sorted, [0, 2], "rows 0 and 2");

    assert!(reader.lookup_name("nonexistent").is_empty());
}

#[test]
fn roaring_prefilter_returns_empty_for_unknown_kind() {
    let (_tmp, seg) = make_segment(&[("foo", "function", 1)]);
    let reader = SegmentReader::open(&seg).expect("open");

    let clauses = clauses_where_kind("nonexistent_kind");
    let result = reader.find_symbols(&clauses, None).expect("find");
    assert!(result.is_empty());
}

#[test]
fn source_path_propagated_to_symbol_match() {
    let (_tmp, seg) = make_segment(&[("main", "function", 1)]);
    let reader = SegmentReader::open(&seg).expect("open");

    let path = std::path::Path::new("src/main.rs");
    let result = reader
        .find_symbols(&Clauses::default(), Some(path))
        .expect("find");
    assert_eq!(result[0].path.as_deref(), Some(path));
}

/// Round-trip: manually build a segment with known content and verify
/// that `find_symbols` with no clauses returns the same rows.
#[test]
fn round_trip_row_content() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let mut b = SegmentBuilder::new("test", &[0xFFu8; 20]);
    let r0 = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 1,
        byte_start: 0,
        byte_end: 50,
        usages_count: 3,
    });
    b.set_field(r0, "is_const", "false");
    let r1 = b.emit_row(SymbolRow {
        name: "beta",
        fql_kind: "struct",
        language: "rust",
        line: 10,
        byte_start: 51,
        byte_end: 200,
        usages_count: 0,
    });
    b.set_field(r1, "member_count", "4");
    b.flush(&seg).expect("flush");

    let reader = SegmentReader::open(&seg).expect("open");

    // Find all, sorted by name.
    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        ..Clauses::default()
    };
    let results = reader.find_symbols(&clauses, None).expect("find");
    assert_eq!(results.len(), 2);

    assert_eq!(results[0].name, "alpha");
    assert_eq!(results[0].fql_kind.as_deref(), Some("function"));
    assert_eq!(results[0].line, Some(1));
    assert_eq!(results[0].usages_count, Some(3));
    assert_eq!(
        results[0].fields.get("is_const").map(String::as_str),
        Some("false")
    );

    assert_eq!(results[1].name, "beta");
    assert_eq!(results[1].fql_kind.as_deref(), Some("struct"));
    assert_eq!(results[1].line, Some(10));
    assert_eq!(
        results[1].fields.get("member_count").map(String::as_str),
        Some("4")
    );

    // ── Gap 4: byte_start_of / byte_end_of accessors ──────────────────
    // r0 = row 0 ("alpha"), r1 = row 1 ("beta") — insertion order.
    assert_eq!(reader.byte_start_of(0), 0, "alpha byte_start");
    assert_eq!(reader.byte_end_of(0), 50, "alpha byte_end");
    assert_eq!(reader.byte_start_of(1), 51, "beta byte_start");
    assert_eq!(reader.byte_end_of(1), 200, "beta byte_end");
}

// ── Gap 5: empty segment ─────────────────────────────────────────────

/// A segment with zero rows must open successfully and return an empty
/// `find_symbols` result without hitting the row-materialisation code.
#[test]
fn find_symbols_on_empty_segment_returns_empty_vec() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let b = SegmentBuilder::new("test", &[0xAAu8; 20]);
    b.flush(&seg).expect("flush");

    let reader = SegmentReader::open(&seg).expect("open");
    assert_eq!(reader.row_count, 0);

    let result = reader
        .find_symbols(&Clauses::default(), None)
        .expect("find on empty segment");
    assert!(result.is_empty(), "expected empty vec for zero-row segment");
}

// ── Gap 3: error-path tests ──────────────────────────────────────────

/// Opening a path that does not exist must return `Err`.
#[test]
fn open_nonexistent_path_returns_err() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let missing = tmp.path().join("does_not_exist.fqsf");
    assert!(
        SegmentReader::open(&missing).is_err(),
        "expected Err for missing file"
    );
}

/// A segment with a corrupted FQSF outer magic must return `Err` at `open`.
#[test]
fn open_corrupt_magic_returns_err() {
    let (_tmp, seg) = make_segment(&[("foo", "function", 1)]);

    // Overwrite the first 4 bytes of the .fqsf file with garbage.
    let mut bytes = std::fs::read(&seg).expect("read segment");
    bytes[0] = b'X';
    bytes[1] = b'X';
    bytes[2] = b'X';
    bytes[3] = b'X';
    std::fs::write(&seg, &bytes).expect("write segment");

    assert!(
        SegmentReader::open(&seg).is_err(),
        "expected Err for corrupt FQSF magic"
    );
}

/// A segment with non-monotone string pool offsets must return `Err` at `open`.
#[test]
fn open_nonmonotone_string_pool_returns_err() {
    // Build a segment with at least two strings so the monotonicity check fires.
    let (_tmp, seg) = make_segment(&[("alpha", "function", 1), ("beta", "struct", 2)]);

    let mut bytes = std::fs::read(&seg).expect("read segment");

    // Find the "strings_offsets" blob in the TOC and corrupt its first two offsets.
    // TOC starts at byte 12; each entry is TOC_ENTRY_SIZE (64) bytes.
    // Entry layout: [name: ENTRY_NAME_LEN bytes][offset: u32 LE][len: u32 LE]
    let entry_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let toc_start = 12;
    for i in 0..entry_count {
        let es = toc_start + i * TOC_ENTRY_SIZE;
        let name_end = bytes[es..es + ENTRY_NAME_LEN]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(ENTRY_NAME_LEN);
        if &bytes[es..es + name_end] == b"strings_offsets" {
            let offset = u32::from_le_bytes(
                bytes[es + ENTRY_NAME_LEN..es + ENTRY_NAME_LEN + 4]
                    .try_into()
                    .unwrap(),
            ) as usize;
            let len = u32::from_le_bytes(
                bytes[es + ENTRY_NAME_LEN + 4..es + ENTRY_NAME_LEN + 8]
                    .try_into()
                    .unwrap(),
            ) as usize;
            // Corrupt: make offset[1] < offset[0] to break monotonicity.
            if len >= 8 {
                let off0 = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
                let bad: u32 = if off0 > 0 { 0 } else { u32::MAX };
                bytes[offset + 4..offset + 8].copy_from_slice(&bad.to_le_bytes());
                std::fs::write(&seg, &bytes).expect("write segment");
                assert!(
                    SegmentReader::open(&seg).is_err(),
                    "expected Err for non-monotone string pool offsets"
                );
            }
            return;
        }
    }
    // blob not found — test passes vacuously (shouldn't happen with real segments)
}

// ── absent columns ────────────────────────────────────────────────────────

/// A segment written before a column existed simply has no TOC entry for it.
/// Resolution at open must turn that into the empty range, so nothing
/// downstream has to notice a missing key.
#[test]
fn a_column_absent_from_the_toc_resolves_to_the_empty_range() {
    let cols = FixedColumns::resolve(&HashMap::new());
    for (label, range) in [
        ("name_id", cols.name_id),
        ("fql_kind_id", cols.fql_kind_id),
        ("language_id", cols.language_id),
        ("line", cols.line),
        ("byte_start", cols.byte_start),
        ("byte_end", cols.byte_end),
        ("usages_count", cols.usages_count),
        ("ordinal", cols.ordinal),
        ("parent_ordinal", cols.parent_ordinal),
        ("rev", cols.rev),
        ("first_child_ordinal", cols.first_child_ordinal),
        ("next_sibling_ordinal", cols.next_sibling_ordinal),
        ("prev_sibling_ordinal", cols.prev_sibling_ordinal),
    ] {
        assert_eq!(
            range,
            (0, 0),
            "absent column {label} must resolve to the empty range"
        );
        assert_eq!(cols.by_short_name(label), (0, 0));
    }
    assert_eq!(cols.by_short_name("not_a_column"), (0, 0));
}

/// Every accessor must answer an absent column with the default it gave when
/// the column was named and looked up per access — `0` / `u32::MAX` / `None`,
/// never a panic. The builder always writes the fixed columns, so no real
/// segment reaches this path; the empty ranges are installed here instead of
/// hoping the corpus contains an old enough segment.
#[test]
fn accessors_on_an_absent_column_return_their_documented_default() {
    let (_tmp, seg) = make_segment(&[("alpha", "function", 7)]);
    let mut reader = SegmentReader::open(&seg).expect("open");
    assert_eq!(
        reader.line_of(0),
        7,
        "control: the column is present to begin with"
    );

    reader.fixed = FixedColumns::resolve(&HashMap::new());
    reader.extra_cols.clear();
    // A column the header names but whose blob never reached the TOC: the name
    // is known, the range is empty, and the value must read as absent.
    reader.extra_cols.push(("phantom".to_owned(), (0, 0)));

    assert_eq!(reader.line_of(0), 0);
    assert_eq!(reader.byte_start_of(0), 0);
    assert_eq!(reader.byte_end_of(0), 0);
    assert_eq!(reader.usages_count_of(0), 0);
    assert_eq!(reader.name_id_of(0), 0);
    assert_eq!(reader.fql_kind_id_of(0), 0);
    assert_eq!(reader.ordinal_of(0), None);
    assert_eq!(reader.rev_of(0), 0);
    assert_eq!(reader.parent_ordinal_of(0), u32::MAX);
    assert_eq!(reader.first_child_ordinal_of(0), u32::MAX);
    assert_eq!(reader.next_sibling_ordinal_of(0), u32::MAX);
    assert_eq!(reader.prev_sibling_ordinal_of(0), u32::MAX);
    assert_eq!(reader.rows_with_language_matching(&|_| true), None);

    // An absent string-id column reads as string id 0 — what the by-name
    // lookup returned when it found no blob. Preserved deliberately.
    assert_eq!(reader.name_of(0), reader.string_of_id(0));
    assert_eq!(reader.fql_kind_of(0), reader.string_of_id(0));
    assert_eq!(reader.language_of(0), reader.string_of_id(0));

    assert!(reader.has_extra_col("phantom"));
    assert_eq!(reader.extra_field_str("phantom", 0), None);
    assert_eq!(reader.extra_field_str("has_doc", 0), None);
    assert_eq!(reader.extra_field_str("line", 0), None);
    assert!(reader.enrichment_for_row(0).is_empty());

    let one = reader
        .materialize_one_row(0, std::path::Path::new("a.rs"))
        .expect("row 0 is in range");
    assert_eq!(one.line, None);
    assert_eq!(one.node_id, None);
    assert_eq!(one.rev, None);
    assert!(one.fields.is_empty());

    let bm: RoaringBitmap = (0u32..1).collect();
    let all = reader.materialize_rows(&bm, Some(std::path::Path::new("a.rs")));
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].line, None);
    assert_eq!(all[0].node_id, None);
    assert_eq!(all[0].rev, None);
    assert!(all[0].fields.is_empty());
}

/// The reader's column enumeration must cover every `col_*` blob the builder
/// writes. `extra_field_str` resolves a name through the enrichment list and
/// then through `FixedColumns::by_short_name`; a core column added to the
/// builder but not to that match arm would answer `None` for every row — a
/// silent absence that compiles clean and that no other test here would catch.
///
/// A column that resolves but holds no bytes is still reachable: its TOC entry
/// starts past the file header, so its range is never `(0, 0)`.
#[test]
fn every_col_blob_the_builder_writes_is_reachable_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let content_id = [0x5C_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    let row = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 1,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    // Two enrichment columns, so the loop below walks the `extra_cols` arm of
    // `col_range` and not only the fixed one.
    b.set_field(row, "param_count", "2");
    b.set_field(row, "has_doc", "true");
    b.flush(&seg).expect("flush");

    let reader = SegmentReader::open(&seg).expect("open");
    assert_eq!(
        reader.extra_col_count(),
        2,
        "fixture must carry enrichment columns"
    );

    let mut checked = 0;
    for name in reader.blobs.keys() {
        let Some(short) = name.strip_prefix("col_") else {
            continue;
        };
        assert_ne!(
            reader.col_range(short),
            (0, 0),
            "TOC holds blob '{name}' but the reader cannot reach column '{short}' by name"
        );
        checked += 1;
    }
    // The builder writes the 13 fixed columns unconditionally, plus one blob
    // per enrichment column. Pinning the sum rather than a floor is what makes
    // this cover both arms: a column added to either side and forgotten by the
    // reader's enumeration shows up here as a count that no longer adds up.
    assert_eq!(
        checked,
        13 + reader.extra_col_count(),
        "every col_* blob in the TOC must be one of the 13 fixed columns or an enrichment column"
    );
}
