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

/// A value lookup answers exactly what the reverse `HashMap` it replaced
/// answered — both directions, the miss included.
///
/// The map is rebuilt here from the pool and kept as the oracle. It is the
/// algorithm `id_of` used until the sorted blob existed, and the property that
/// matters is not that the search works but that it decides the same segments
/// in and out: `None` is what lets a prefilter skip a segment, so a wrong miss
/// is a row that never comes back.
#[test]
fn a_value_lookup_answers_what_the_reverse_map_answered() {
    let (_tmp, seg) = make_segment(&[
        ("alpha", "function", 1),
        ("beta", "struct", 2),
        ("Gamma", "function", 3),
        ("alpha2", "function", 4),
    ]);
    let reader = SegmentReader::open(&seg).expect("open");
    let pool = &reader.strings;

    let oracle: HashMap<String, u32> = (0..pool.string_count)
        .map(|id| (pool.get(id).to_owned(), id))
        .collect();
    assert!(
        oracle.len() >= 4,
        "fixture must intern several strings, got {}",
        oracle.len()
    );

    for (s, &id) in &oracle {
        assert_eq!(
            pool.id_of(s),
            Some(id),
            "the search and the map disagree on the present {s:?}"
        );
    }

    // Probes chosen to be the shapes a binary search gets wrong when the
    // ordering is not the one the writer used: either side of a present
    // string, a prefix of one, an extension of one, and a case fold.
    for absent in ["", "aaa", "zzz", "alph", "alphaa", "ALPHA", "gamma"] {
        assert_eq!(
            pool.id_of(absent),
            oracle.get(absent).copied(),
            "the search and the map disagree on {absent:?}"
        );
    }
}

/// The writer's `strings_sorted` blob is a permutation of the pool's ids in
/// the order of the bytes they name.
///
/// That is the invariant the binary search rests on and the one thing no
/// lookup re-checks: an unsorted blob would not fail, it would answer `None`
/// for a string the segment holds.
#[test]
fn the_sorted_string_index_is_a_permutation_in_byte_order() {
    let (_tmp, seg) = make_segment(&[
        ("alpha", "function", 1),
        ("beta", "struct", 2),
        ("Gamma", "function", 3),
        ("alpha2", "function", 4),
    ]);
    let reader = SegmentReader::open(&seg).expect("open");
    let pool = &reader.strings;

    let ids: &[u32] = cast_slice(&pool.mmap[pool.srt_start..pool.srt_end]);
    assert_eq!(
        ids.len(),
        pool.string_count as usize,
        "the blob holds one id per string"
    );

    for pair in ids.windows(2) {
        let (lo, hi) = (pool.get(pair[0]), pool.get(pair[1]));
        assert!(
            lo < hi,
            "ids {} and {} are out of byte order: {lo:?} then {hi:?}",
            pair[0],
            pair[1]
        );
    }

    let mut seen = vec![false; ids.len()];
    for &id in ids {
        assert!(!seen[id as usize], "id {id} appears twice");
        seen[id as usize] = true;
    }
    assert!(seen.iter().all(|&s| s), "every id of the pool appears once");
}

/// A segment carrying no `strings_sorted` blob is refused at open rather than
/// served from a fallback.
///
/// Nothing can produce one any more: segments are cached under a directory
/// named for `ENRICH_VER` and the blob landed with a bump, so the only way to
/// ask the reader this question is to take the blob away from a segment it
/// wrote. Keeping the map beside the search for such a file would be keeping a
/// path no build can reach and no test can exercise.
#[test]
fn a_segment_without_the_sorted_string_index_is_refused_at_open() {
    let (_tmp, seg) = make_segment(&[("alpha", "function", 1), ("beta", "struct", 2)]);
    let mut bytes = std::fs::read(&seg).expect("read segment");

    // Rename the entry rather than dropping it: every other blob stays exactly
    // where the table says it is, so the open fails for the one reason under
    // test. `parse_toc` sorts what it reads, so the renamed entry is still
    // found by every other name.
    let entry_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    let mut renamed = false;
    for i in 0..entry_count {
        let es = 12 + i * TOC_ENTRY_SIZE;
        let name_end = bytes[es..es + ENTRY_NAME_LEN]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(ENTRY_NAME_LEN);
        if &bytes[es..es + name_end] == b"strings_sorted" {
            bytes[es] = b'x';
            renamed = true;
        }
    }
    assert!(renamed, "the builder must write a strings_sorted blob");
    std::fs::write(&seg, &bytes).expect("write segment");

    let Err(err) = SegmentReader::open(&seg) else {
        panic!("a segment with no strings_sorted blob opened instead of being refused");
    };
    let text = format!("{err:#}");
    assert!(
        text.contains("strings_sorted"),
        "the refusal must name the missing blob, got: {text}"
    );
}

// ── absent columns ────────────────────────────────────────────────────────

/// A segment written before a column existed simply has no TOC entry for it.
/// Resolution at open must turn that into the empty range, so nothing
/// downstream has to notice a missing key.
#[test]
fn a_column_absent_from_the_toc_resolves_to_the_empty_range() {
    let cols = FixedColumns::resolve(&Toc::default());
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
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let mut b = SegmentBuilder::new("test", &[0xAB_u8; 20]);
    let row = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 7,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    // Named in the header, so the reader carries a real column name; the blob
    // behind it is taken away below.
    b.set_field(row, "phantom", "present");
    b.flush(&seg).expect("flush");

    let mut reader = SegmentReader::open(&seg).expect("open");
    assert_eq!(
        reader.line_of(0),
        7,
        "control: the column is present to begin with"
    );
    assert_eq!(
        reader.extra_field_str("phantom", 0),
        Some("present"),
        "control: the enrichment column is present to begin with"
    );

    // A column the header names but whose blob never reached the TOC: the name
    // is known, the range is empty, and the value must read as absent.
    let name = Arc::clone(&reader.extra_cols[0].name);
    reader.fixed = FixedColumns::resolve(&Toc::default());
    reader.set_extra_cols(vec![ExtraCol { name, data: (0, 0) }]);

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
    // The reader keeps no name table of its own, so read the table of contents
    // back off the file the builder wrote. That is the stronger check anyway:
    // it asks the bytes on disk what blobs exist, not the reader's idea of it.
    let bytes = std::fs::read(&seg).expect("read segment");
    let toc = parse_toc(&bytes, bytes.len(), &seg).expect("parse toc");
    for entry in toc.entries() {
        let Some(short) = entry.name.strip_prefix("col_") else {
            continue;
        };
        assert_ne!(
            reader.col_range(short),
            (0, 0),
            "TOC holds blob '{}' but the reader cannot reach column '{short}' by name",
            entry.name
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

// ── row view: the pre-materialisation filter ─────────────────────────────

/// Three rows chosen so that every arm of `RowField` is exercised on a row
/// that has a value and on one that does not: `beta` has no `param_count` and
/// line `0`, `gamma` has neither a kind nor a language.
fn segment_for_row_view() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let content_id = [0x71_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    let r0 = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 12,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    b.set_field(r0, "param_count", "2");
    let _r1 = b.emit_row(SymbolRow {
        name: "beta",
        fql_kind: "struct",
        language: "rust",
        line: 0,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    let r2 = b.emit_row(SymbolRow {
        name: "gamma",
        fql_kind: "",
        language: "",
        line: 99,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    b.set_field(r2, "param_count", "10");
    b.flush(&seg).expect("flush");
    (tmp, seg)
}

fn predicate(field: &str, op: CompareOp, value: PredicateValue) -> Predicate {
    Predicate {
        field: field.to_owned(),
        op,
        value,
    }
}

/// The claim the whole pre-materialisation filter rests on: a predicate
/// answered from the columns answers exactly what the row it would have built
/// answers. A disagreement in one direction returns a row that should have
/// been filtered out; in the other it loses a row for good, which no later
/// stage can recover. So both sides are driven over every arm and every
/// operator the split lets through, and any difference fails here.
#[test]
fn a_row_view_answers_a_prefilterable_predicate_as_the_built_row_does() {
    let (_tmp, seg) = segment_for_row_view();
    let reader = SegmentReader::open(&seg).expect("open");
    let path = PathBuf::from("src/lib.rs");
    let all: RoaringBitmap = (0..reader.row_count).collect();
    let built = reader.materialize_rows(&all, Some(&path));
    assert_eq!(built.len(), 3, "fixture must materialise every row");

    let probes = vec![
        predicate(
            "name",
            CompareOp::Eq,
            PredicateValue::String("alpha".to_owned()),
        ),
        predicate(
            "name",
            CompareOp::NotEq,
            PredicateValue::String("alpha".to_owned()),
        ),
        predicate(
            "name",
            CompareOp::Like,
            PredicateValue::String("%a".to_owned()),
        ),
        predicate(
            "name",
            CompareOp::NotLike,
            PredicateValue::String("%a".to_owned()),
        ),
        predicate(
            "fql_kind",
            CompareOp::Eq,
            PredicateValue::String("function".to_owned()),
        ),
        // The empty string is how a segment spells "this row has no kind", and
        // both the view and the built row now report it as the VALUE it is
        // rather than as absent — which is what keeps them agreeing here.
        predicate(
            "fql_kind",
            CompareOp::Eq,
            PredicateValue::String(String::new()),
        ),
        predicate(
            "language",
            CompareOp::Eq,
            PredicateValue::String("rust".to_owned()),
        ),
        predicate(
            "path",
            CompareOp::Eq,
            PredicateValue::String("src/lib.rs".to_owned()),
        ),
        predicate(
            "path",
            CompareOp::Like,
            PredicateValue::String("src/%".to_owned()),
        ),
        // Line `0` is likewise absence, not a line number.
        predicate("line", CompareOp::Eq, PredicateValue::Number(0)),
        predicate("line", CompareOp::Gte, PredicateValue::Number(12)),
        predicate("line", CompareOp::Lte, PredicateValue::Number(12)),
        predicate("line", CompareOp::Gt, PredicateValue::Number(0)),
        predicate("line", CompareOp::Lt, PredicateValue::Number(99)),
        predicate("param_count", CompareOp::Eq, PredicateValue::Number(2)),
        predicate("param_count", CompareOp::Gte, PredicateValue::Number(2)),
        predicate("param_count", CompareOp::Lt, PredicateValue::Number(10)),
        predicate(
            "param_count",
            CompareOp::Eq,
            PredicateValue::String("2".to_owned()),
        ),
        predicate("param_count", CompareOp::NotEq, PredicateValue::Number(2)),
    ];

    for p in &probes {
        let field = crate::field_tiers::canonical(&p.field);
        assert!(
            reader.answers_field(
                field,
                true,
                crate::storage::columnar::segment_reader::accessor_for(p)
            ),
            "probe '{field}' is not prefilterable, so this test would prove nothing about it"
        );
        for (row, built_row) in built.iter().enumerate() {
            let row = u32::try_from(row).expect("row index");
            let view = SegRowRef {
                seg: &reader,
                row,
                source_path: Some(&path),
            };
            assert_eq!(
                crate::filter::eval_predicate(&view, p),
                crate::filter::eval_predicate(built_row, p),
                "row {row} disagrees on {} {:?} {:?}",
                p.field,
                p.op,
                p.value
            );
        }
    }
}

/// A field a built row answers from a struct field of its own is never
/// answered from a column, because the column and the struct do not agree:
/// `usages` is overwritten from the workspace overlay after materialisation,
/// `node_id` is built during it, `node_kind` is not stored at all, and `count`
/// is assigned later still by GROUP BY.
///
/// A field nothing holds is the opposite case and is answered, so the two are
/// checked together: declining is for where the readers would disagree, not
/// for wherever a column is missing.
#[test]
fn fields_a_built_row_answers_from_its_struct_are_not_answered_from_columns() {
    let (_tmp, seg) = segment_for_row_view();
    let reader = SegmentReader::open(&seg).expect("open");
    for field in ["usages", "node_id", "node_kind", "count"] {
        assert!(
            !reader.answers_field(field, true, Accessor::Str)
                && !reader.answers_field(field, true, Accessor::Num),
            "'{field}' must fall back to the filter that runs on built rows"
        );
    }
    // `path` is the caller's, so it is answerable only when the caller gave one.
    assert!(reader.answers_field("path", true, Accessor::Str));
    assert!(!reader.answers_field("path", false, Accessor::Str));
    // A field no column of this segment holds IS answered — by reporting that
    // it is absent. The row this segment would build carries it in neither its
    // struct nor its enrichment map, so both readers resolve it to None and
    // every operator that consults the field is false on both. That is a
    // different case from the four above, where the built row does carry the
    // field and reads it from somewhere no view can see.
    assert!(reader.answers_field("has_doc", true, Accessor::Str));
}

/// An enrichment column named after one of those fields is followed rather than
/// refused, because the built row follows it too.
///
/// The two accessors go different ways on such a name: a string operator on
/// `name` reads the struct on a built row, a numeric one falls through to its
/// enrichment map and finds the shadow column. This reader is told which
/// accessor is coming, so it reads the fixed column for the first and the same
/// shadow column for the second, and agrees with the built row on both. It used
/// to be told neither and declined the name outright. The rest of the segment
/// was never in question: every field the column does not name is answered from
/// its own column, as the built row answers it.
#[test]
fn an_enrichment_column_named_after_a_struct_field_is_followed_not_refused() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let content_id = [0x33_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    let row = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 12,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    // `name` is the shadow to use: it is answered from a fixed column under one
    // accessor and from this enrichment column under the other, so following it
    // is observable both ways. A column named `usages` would prove nothing —
    // `usages` is unanswerable under either accessor whatever this reader does.
    b.set_field(row, "name", "77");
    b.set_field(row, "param_count", "2");
    b.flush(&seg).expect("flush");

    let reader = SegmentReader::open(&seg).expect("open");

    // THE MECHANISM. A column named `name` no longer withholds anything: a
    // string operator reads the fixed name column, which is where the built row
    // reads it too, and a numeric one reads this shadow column, which is where
    // the built row reads THAT. If this ever goes back to being refused, the
    // scan silently returns to building every row of every segment carrying a
    // grammar field called `name` — 292 of 293 on this repository's own index —
    // and no golden case would see it, because the answers are the same either
    // way.
    assert!(
        reader.answers_field("name", true, Accessor::Str),
        "a shadowed name is still answered from the fixed column for a string \
         operator, because that is the column the built row answers from"
    );
    assert!(
        reader.answers_field("name", true, Accessor::Num),
        "and from the shadow column for a numeric one, for the same reason"
    );
    for field in ["fql_kind", "language", "line", "path"] {
        assert!(
            reader.answers_field(field, true, Accessor::Str),
            "'{field}' does not collide with the 'name' column, so it is still answered"
        );
    }
    assert!(
        reader.answers_field("param_count", true, Accessor::Num),
        "a column that collides with nothing is still answered"
    );

    let path = PathBuf::from("src/lib.rs");
    let all: RoaringBitmap = (0..reader.row_count).collect();
    let built = reader.materialize_rows(&all, Some(&path));
    let view = SegRowRef {
        seg: &reader,
        row: 0,
        source_path: Some(&path),
    };

    // Answering the rest early is only sound while it agrees with the built row.
    let probes = [
        predicate(
            "fql_kind",
            CompareOp::Eq,
            PredicateValue::String("function".to_owned()),
        ),
        predicate(
            "language",
            CompareOp::Eq,
            PredicateValue::String("rust".to_owned()),
        ),
        predicate("line", CompareOp::Eq, PredicateValue::Number(12)),
        predicate(
            "path",
            CompareOp::Like,
            PredicateValue::String("%lib.rs".to_owned()),
        ),
        predicate("param_count", CompareOp::Eq, PredicateValue::Number(2)),
    ];
    for p in &probes {
        assert!(
            reader.answers_field(
                crate::field_tiers::canonical(&p.field),
                true,
                accessor_for(p)
            ),
            "probe '{}' is not answered early, so it would prove nothing here",
            p.field
        );
        assert_eq!(
            crate::filter::eval_predicate(&view, p),
            crate::filter::eval_predicate(&built[0], p),
            "row view and built row disagree on '{}' in a segment carrying a shadowed column",
            p.field
        );
    }

    // And the arm that used to force the whole name to be withheld from THIS
    // reader. A built row reads `name` from its struct for a string operator,
    // but a NUMERIC one falls through to the enrichment map and finds the
    // shadow column there. `SegRowRef` used to answer from the fixed column or
    // not at all, so it could not follow, and the safe move was to decline
    // `name` on this segment under every operator.
    //
    // It follows now: the accessor is part of what it is asked, so the numeric
    // operator reads the shadow column here as well and the two agree. That is
    // what lets the string operators above be answered before the rows exist.
    //
    // `RowView`, the reader that ranks and keys a row rather than filtering it,
    // has mirrored the built row on both operators all along — see
    // `a_view_reads_every_field_as_the_row_it_builds`. The two readers now
    // resolve a field the same way; what still differs is only what they are
    // used for.
    let cross_type = predicate("name", CompareOp::Eq, PredicateValue::Number(77));
    assert!(
        crate::filter::eval_predicate(&built[0], &cross_type),
        "a numeric operator on 'name' reads the shadow column on a built row"
    );
    assert_eq!(
        crate::filter::eval_predicate(&view, &cross_type),
        crate::filter::eval_predicate(&built[0], &cross_type),
        "the row view and the built row must read the shadow column alike under \
         a numeric operator, or answering the string ones early is unsound"
    );
}

/// `STRUCT_BACKED_FIELDS` is a hand-written list, and the argument that it is
/// complete is not restated here — it is derived and re-derived on every run.
///
/// For every field name a symbol row declares, a segment is built whose
/// enrichment column carries that very name with a value the row's own struct
/// field cannot hold. Wherever the row view claims to answer such a field, its
/// answer must equal the built row's. A struct arm added to `SymbolMatch`
/// later without a matching entry in `STRUCT_BACKED_FIELDS` would be read from
/// the column early and from the struct late, and the two would disagree here.
#[test]
fn a_column_named_after_any_declared_field_answers_as_the_built_row_or_not_at_all() {
    // Fixed columns are stored as `col_<name>` too, so a declared field that
    // collides with one can never be given an enrichment column. Learn which
    // names those are from a real segment rather than listing them again.
    let (_probe_tmp, probe) = make_segment(&[("probe", "function", 1)]);
    let probe_bytes = std::fs::read(&probe).expect("read probe segment");
    let probe_toc = parse_toc(&probe_bytes, probe_bytes.len(), &probe).expect("parse probe toc");
    let fixed_blobs: Vec<&str> = probe_toc.entries().map(|entry| entry.name).collect();

    let declared: Vec<&str> = <SymbolMatch as crate::filter::ClauseTarget>::STR_FIELDS
        .iter()
        .chain(<SymbolMatch as crate::filter::ClauseTarget>::NUM_FIELDS)
        .copied()
        .collect();
    assert!(!declared.is_empty(), "a symbol row declares field names");

    let path = PathBuf::from("src/lib.rs");
    let mut compared = 0_usize;
    for field in declared {
        if fixed_blobs.contains(&format!("col_{field}").as_str()) {
            // The shadowing case cannot arise for this name, but the fixed
            // answer it stands for must still be one the guard covers.
            assert!(
                STRUCT_BACKED_FIELDS.contains(&field),
                "'{field}' names a fixed column but no guard covers it"
            );
            continue;
        }

        let tmp = tempfile::tempdir().expect("tempdir");
        let seg = tmp.path().join("seg.fqsf");
        let content_id = [0x5E_u8; 20];
        let mut b = SegmentBuilder::new("test", &content_id);
        let row = b.emit_row(SymbolRow {
            name: "alpha",
            fql_kind: "function",
            language: "rust",
            line: 12,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        b.set_field(row, field, "77");
        b.flush(&seg).expect("flush");

        let reader = SegmentReader::open(&seg).expect("open");
        let all: RoaringBitmap = (0..reader.row_count).collect();
        let built = reader.materialize_rows(&all, Some(&path));
        let view = SegRowRef {
            seg: &reader,
            row: 0,
            source_path: Some(&path),
        };

        let probes = [
            predicate(
                field,
                CompareOp::Eq,
                PredicateValue::String("77".to_owned()),
            ),
            predicate(field, CompareOp::Eq, PredicateValue::Number(77)),
            predicate(field, CompareOp::Gte, PredicateValue::Number(77)),
            predicate(
                field,
                CompareOp::Like,
                PredicateValue::String("%7".to_owned()),
            ),
        ];
        for p in &probes {
            let canonical = crate::field_tiers::canonical(&p.field);
            if !reader.answers_field(canonical, true, accessor_for(p)) {
                continue;
            }
            compared += 1;
            assert_eq!(
                crate::filter::eval_predicate(&view, p),
                crate::filter::eval_predicate(&built[0], p),
                "a column named '{field}' is answered early but not as the built row answers it"
            );
        }
    }

    assert!(
        compared > 0,
        "no declared field was actually answered early, so this proved nothing"
    );
}

/// `enrichment_columns` is `enrichment_for_row` turned inside out: the overlay
/// build reads enrichment through the first, every query reads it through the
/// second.  If the two ever disagree about which slots count — the `u32::MAX`
/// NULL, the empty string, a column absent from this segment — the overlay
/// quietly stops describing what a query sees, and no query result changes to
/// say so.  This is the only place that disagreement is visible.
#[test]
fn enrichment_columns_agrees_with_enrichment_for_row() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let content_id = [0x3C_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);

    for (i, name) in ["alpha", "beta", "gamma"].iter().enumerate() {
        let row = b.emit_row(SymbolRow {
            name,
            fql_kind: "function",
            language: "rust",
            line: u32::try_from(i + 1).expect("small fixture"),
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        // Row 1 leaves `param_count` unset and carries an empty `naming` — the
        // two slot kinds the two views have to agree to skip.
        if i != 1 {
            b.set_field(row, "param_count", "2");
        }
        b.set_field(row, "naming", if i == 1 { "" } else { "snake_case" });
    }
    b.flush(&seg).expect("flush");

    let reader = SegmentReader::open(&seg).expect("open");
    for row in 0..reader.row_count {
        let mut from_columns: HashMap<String, String> = HashMap::new();
        for (name, value_ids) in reader.enrichment_columns() {
            let Some(&id) = value_ids.get(row as usize) else {
                continue;
            };
            if id == u32::MAX {
                continue;
            }
            let value = reader.string_of_id(id);
            if value.is_empty() {
                continue;
            }
            let _ = from_columns.insert(name.to_owned(), value.to_owned());
        }
        assert_eq!(
            from_columns,
            reader.enrichment_for_row(row),
            "the column view and the row view disagree at row {row}"
        );
    }
}

// ── posting blobs read in place ──────────────────────────────────────────

/// `(offset, len)` of the TOC entry named `name`, or a panic naming it.
fn toc_range(bytes: &[u8], name: &[u8]) -> (usize, usize) {
    let entry_count = u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    for i in 0..entry_count {
        let es = 12 + i * TOC_ENTRY_SIZE;
        let name_end = bytes[es..es + ENTRY_NAME_LEN]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(ENTRY_NAME_LEN);
        if &bytes[es..es + name_end] == name {
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
            return (offset, len);
        }
    }
    panic!("no TOC entry named {}", String::from_utf8_lossy(name));
}

/// A segment with two kinds, one posted flag, and names sharing a prefix.
fn segment_with_postings() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let mut b = SegmentBuilder::new("test", &[0xCDu8; 20]);
    let mut row = |name: &'static str, kind: &'static str, line: u32| {
        b.emit_row(SymbolRow {
            name,
            fql_kind: kind,
            language: "rust",
            line,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        })
    };
    let alpha = row("alpha", "function", 1);
    let _also = row("also", "function", 2);
    let _beta = row("beta", "struct", 3);
    b.set_field(alpha, "is_const", "true");
    b.flush(&seg).expect("flush");
    (tmp, seg)
}

/// Every posting lookup answers from the mmap what the builder wrote: the
/// kind walk partitions the rows by their stored kind, a posted flag names
/// exactly the row it was set on, and the prefix index finds the names.
#[test]
fn posting_lookups_answer_what_the_builder_wrote() {
    let (_tmp, seg) = segment_with_postings();
    let reader = SegmentReader::open(&seg).expect("open");

    let mut covered = RoaringBitmap::new();
    for entry in reader.kind_postings() {
        let (kind_id, rows) = entry.expect("kind entry");
        assert!(!rows.is_empty(), "a posted kind has rows");
        for row in &rows {
            assert_eq!(
                reader.fql_kind_id_of(row),
                kind_id,
                "row {row} posted under another kind"
            );
        }
        assert_eq!(
            reader.kind_rows(kind_id).expect("kind_rows").as_ref(),
            Some(&rows),
            "the walk and the keyed lookup disagree for kind {kind_id}"
        );
        covered |= rows;
    }
    assert_eq!(
        covered.len(),
        u64::from(reader.row_count),
        "every row is posted under its kind"
    );
    let unknown_kind = u32::MAX;
    assert_eq!(reader.kind_rows(unknown_kind).expect("kind_rows"), None);

    assert!(reader.posts_field("is_const"));
    assert!(
        !reader.posts_field("has_doc"),
        "a flag never set on any row is not posted"
    );
    let true_id = reader.strings.id_of("true").expect("'true' is in the pool");
    let const_rows = reader
        .field_rows("is_const", true_id)
        .expect("field_rows")
        .expect("is_const=true is posted");
    assert_eq!(
        const_rows.iter().collect::<Vec<_>>(),
        vec![0],
        "only alpha is const"
    );
    let walked: Vec<(u32, RoaringBitmap)> = reader
        .field_postings("is_const")
        .collect::<Result<_>>()
        .expect("field walk");
    assert_eq!(walked, vec![(true_id, const_rows)]);
    assert_eq!(
        reader.field_postings("has_doc").count(),
        0,
        "an unposted field walks empty"
    );
    assert!(reader.proves_enrichment_value_absent("is_const", "false"));
    assert!(!reader.proves_enrichment_value_absent("is_const", "true"));

    assert!(reader.has_name_prefix_index());
    let al = reader
        .name_prefix_rows(b"al")
        .expect("name_prefix_rows")
        .expect("two names start with 'al'");
    assert_eq!(al.iter().collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(
        reader.name_prefix_rows(b"zz").expect("name_prefix_rows"),
        None
    );
}

/// A bitmap whose bytes will not decode is reported by the lookup that
/// reaches it — never at open, and never as "no rows": the kind prefilter and
/// the enrichment prefilter both stop narrowing and the residual filter still
/// produces the right answer, and the absence proof declines to prove.
#[test]
fn a_corrupt_posting_bitmap_is_reported_at_lookup_and_never_narrows() {
    let (_tmp, seg) = segment_with_postings();
    let mut bytes = std::fs::read(&seg).expect("read segment");
    for blob in [&b"postings_fql_kind"[..], &b"postings_is_const"[..]] {
        let (offset, len) = toc_range(&bytes, blob);
        assert!(
            len > 12,
            "{} blob has an entry to corrupt",
            String::from_utf8_lossy(blob)
        );
        // [entry_count][id][bitmap_len][bitmap…]* — clobber every bitmap's
        // header (its serial cookie) so `deserialize_from` refuses each one.
        let entry_count =
            u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        let mut pos = offset + 4;
        for _ in 0..entry_count {
            let bitmap_len =
                u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
            let bitmap_start = pos + 8;
            bytes[bitmap_start..bitmap_start + 4].copy_from_slice(&[0xFF; 4]);
            pos = bitmap_start + bitmap_len;
        }
    }
    std::fs::write(&seg, &bytes).expect("write segment");

    let reader = SegmentReader::open(&seg).expect("a corrupt bitmap does not refuse the open");

    let function_id = reader.strings.id_of("function").expect("kind in pool");
    assert!(
        reader.kind_rows(function_id).is_err(),
        "the lookup reports the corruption"
    );
    // The prefilter cannot narrow, so every row is a candidate and the
    // residual filter decides — the answer is still exactly the functions.
    let found = reader
        .find_symbols(&clauses_where_kind("function"), None)
        .expect("find");
    assert_eq!(names(&found), vec!["alpha", "also"]);

    let true_id = reader.strings.id_of("true").expect("'true' in pool");
    assert!(reader.field_rows("is_const", true_id).is_err());
    let all: RoaringBitmap = (0..reader.row_count).collect();
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "is_const".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("true".to_owned()),
        }],
        ..Clauses::default()
    };
    assert_eq!(
        reader.prefilter_enrichment_postings(all.clone(), &clauses),
        all,
        "an unreadable bitmap leaves the predicate to the residual filter"
    );
    assert!(
        !reader.proves_enrichment_value_absent("is_const", "true"),
        "an unreadable bitmap proves nothing"
    );
}

/// An entry table that runs past its blob is a layout error, and `open`
/// still refuses it — that check did not move with the decode.
#[test]
fn a_truncated_posting_table_is_refused_at_open() {
    let (_tmp, seg) = segment_with_postings();
    let mut bytes = std::fs::read(&seg).expect("read segment");
    let (offset, _) = toc_range(&bytes, b"postings_fql_kind");
    bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    std::fs::write(&seg, &bytes).expect("write segment");
    let Err(err) = SegmentReader::open(&seg) else {
        panic!("an entry count past the blob is refused");
    };
    assert!(
        format!("{err:#}").contains("postings_fql_kind"),
        "the refusal names the blob: {err:#}"
    );
}

/// The segment-level language check, and the case that keeps it honest.
///
/// One comparison per segment is only sound while a segment holds one language.
/// That is normally true — a segment is built from one source path — but it is
/// checked rather than assumed, and a segment that broke the rule has to
/// DECLINE rather than answer from its first row. A caller that took the first
/// row's word for it would drop the other language's rows from the page in
/// silence: the very failure the stamp-only defaults exist to prevent,
/// reappearing one layer down.
#[test]
fn a_mixed_language_segment_declines_the_segment_level_check() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let mut b = SegmentBuilder::new("test", &[0x21_u8; 20]);
    for (name, language) in [("foo", "cpp"), ("bar", "python")] {
        b.add_row(SymbolRow {
            name,
            fql_kind: "function",
            language,
            line: 1,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
    }
    b.flush(&seg).expect("flush");
    let reader = SegmentReader::open(&seg).expect("open");

    assert_eq!(
        reader.segment_written_in(&["cpp"]),
        None,
        "a segment holding two languages cannot be decided by one comparison"
    );
    assert_eq!(
        reader.segment_written_in(&["cpp", "python"]),
        None,
        "a list covering both languages must not rescue it: the question is whether \
         ONE comparison can decide the segment, not what the answer would be"
    );

    // The exact per-row reader beside it, which CAN decide such a segment. It is
    // what a caller falls back to — or rather, it is the tier the caller falls
    // back through — so the two must not both be blind here.
    let cpp_rows = reader
        .rows_with_language_matching(&|stored| stored == "cpp")
        .expect("the per-row reader decides a mixed segment");
    assert_eq!(cpp_rows.len(), 1, "one of the two rows is cpp");
}

/// The two answers the check does give, on the uniform segments it is for.
#[test]
fn a_uniform_segment_answers_the_segment_level_check_both_ways() {
    let (_tmp, seg) = make_segment(&[("foo", "function", 1), ("bar", "function", 5)]);
    let reader = SegmentReader::open(&seg).expect("open");

    assert_eq!(reader.segment_written_in(&["rust", "cpp"]), Some(true));
    assert_eq!(reader.segment_written_in(&["cpp"]), Some(false));
    assert_eq!(
        reader.segment_written_in(&[]),
        Some(false),
        "an empty language list covers nothing, and that is an answer rather than a refusal"
    );
}

/// A segment whose rows carry no language is DECIDED, not declined.
///
/// Every row carries the same value, so one comparison settles it; the answer
/// is that no list covers it. That matches the row-level evaluator, which
/// answers nothing for a row carrying no language, so both readers exclude the
/// same rows for the same reason and neither has to guess.
#[test]
fn a_segment_with_no_language_is_decided_rather_than_declined() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let seg = tmp.path().join("seg.fqsf");
    let mut b = SegmentBuilder::new("test", &[0x22_u8; 20]);
    b.add_row(SymbolRow {
        name: "foo",
        fql_kind: "function",
        language: "",
        line: 1,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    b.flush(&seg).expect("flush");
    let reader = SegmentReader::open(&seg).expect("open");

    assert_eq!(reader.segment_written_in(&["cpp", "rust", ""]), Some(false));
}
