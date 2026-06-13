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
