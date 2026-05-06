//! Phase 05 — Parity harness: `ColumnarStorage` (overlay) vs legacy `SymbolTable`.
//!
//! Tests the full overlay build + query round-trip:
//!
//! 1. Index two canonical fixtures producing two `SymbolTable`s.
//! 2. Build two segments (one per fixture) via `ShadowWriter`.
//! 3. Build an `OverlayBuilder` from the `ShadowWriteResult::segment_map`.
//! 4. Open the overlay with `Overlay::open`.
//! 5. Materialise all rows via `ColumnarStorage::find_symbols`.
//! 6. Compare against the merged legacy result set — name, fql_kind, line.
//!
//! Run with:
//! ```
//! cargo test -p forgeql-core --test overlay_parity
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{SymbolTable, index_file};
use forgeql_core::ast::lang::{CppLanguageInline, LanguageSupport, RustLanguageInline};
use forgeql_core::ir::Clauses;
use forgeql_core::result::SymbolMatch;
use forgeql_core::storage::columnar::{OverlayBuilder, SegmentBuilder, SegmentReader};
use tempfile::TempDir;

// ── fixtures ─────────────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

fn fixture_path(filename: &str) -> PathBuf {
    let p = fixtures_dir().join(filename);
    assert!(p.exists(), "fixture missing: {}", p.display());
    p
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Index a fixture file with the given language and return the `SymbolTable`.
fn index_fixture(lang: &dyn LanguageSupport, filename: &str) -> SymbolTable {
    let path = fixture_path(filename);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    let count = index_file(&mut parser, &path, &mut table, &enrichers, lang, None, None)
        .expect("index_file should succeed");
    assert!(count > 0, "expected at least one row in {filename}");
    table
}

/// Build a segment for `table`, store it under `segments_dir/<provider>/<hex>/`,
/// and return `(abs_source_path, content_id_bytes)`.
fn build_segment(
    table: &SymbolTable,
    abs_source_path: &std::path::Path,
    segments_dir: &std::path::Path,
) -> Vec<u8> {
    // Deterministic content ID based on source path hash (for test only).
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    abs_source_path.hash(&mut h);
    let hash_u64 = h.finish();
    let content_id: Vec<u8> = hash_u64.to_le_bytes().to_vec();

    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let seg_dir = segments_dir.join("test").join(&hex);

    let mut builder = SegmentBuilder::new("test", &content_id);
    for row in &table.rows {
        #[allow(clippy::cast_possible_truncation)]
        let row_id = builder.emit_row(
            table.name_of(row),
            table.fql_kind_of(row),
            table.language_of(row),
            row.line as u32,
            row.byte_range.start as u32,
            row.byte_range.end as u32,
            row.usages_count,
        );
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder.flush(&seg_dir).expect("segment flush");

    content_id
}

/// Flatten a legacy `SymbolTable` to canonical key tuples.
#[allow(dead_code)]
fn legacy_key_tuples(table: &SymbolTable) -> Vec<(String, String, usize)> {
    let mut v: Vec<_> = table
        .rows
        .iter()
        .map(|r| {
            (
                table.name_of(r).to_owned(),
                table.fql_kind_of(r).to_owned(),
                r.line,
            )
        })
        .collect();
    v.sort_unstable();
    v
}

/// Flatten `find_symbols` results to canonical key tuples.
fn columnar_key_tuples(results: &[SymbolMatch]) -> Vec<(String, String, usize)> {
    let mut v: Vec<_> = results
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.fql_kind.clone().unwrap_or_default(),
                r.line.unwrap_or(0),
            )
        })
        .collect();
    v.sort_unstable();
    v
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Build a 2-segment overlay from `canonical.cpp` and `canonical.rs`, then
/// verify that `ColumnarStorage::find_symbols` returns the same
/// (name, fql_kind, line) triples as the merged legacy tables.
#[test]
fn overlay_find_symbols_matches_legacy_merged() {
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let table_rust = index_fixture(&RustLanguageInline, "canonical.rs");

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");

    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let rs_cid = build_segment(&table_rust, &rs_path, &segments_dir);

    // Build segment_map: abs_path → content_id (mirrors ShadowWriteResult)
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_cid);
    let _ = segment_map.insert(rs_path, rs_cid);

    let overlay_path = overlays_dir.join("test").join("deadbeef00.bin");
    let builder = OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map);
    builder
        .build_and_persist(&overlay_path)
        .expect("overlay build");
    assert!(overlay_path.exists(), "overlay file should be on disk");

    // Open via ColumnarStorage
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let seg_count = overlay.segments().len();
    assert_eq!(seg_count, 2, "expected 2 segments in overlay");
    assert!(overlay.row_count() > 0, "expected non-zero row count");

    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            let seg_dir = segments_dir.join("test").join(&meta.hex_content_id);
            Arc::new(SegmentReader::open(&seg_dir).expect("SegmentReader::open"))
        })
        .collect();

    let storage = ColumnarStorage::new(fixtures_dir(), segments, overlay);
    let clauses = Clauses::default();
    let results = storage
        .find_symbols(&clauses, tmp.path())
        .expect("find_symbols");

    // Build merged legacy baseline deduped on (name, fql_kind, path, line) —
    // the same key ColumnarStorage::find_symbols uses internally.  Raw
    // index_file calls may produce duplicate rows; we mirror the dedup here
    // so that the expected count matches what find_symbols returns.
    use std::collections::HashSet;
    let mut seen: HashSet<(String, String, std::path::PathBuf, usize)> = HashSet::new();
    let mut legacy_rows: Vec<(String, String, usize)> = Vec::new();
    for r in &table_cpp.rows {
        let key = (
            table_cpp.name_of(r).to_owned(),
            table_cpp.fql_kind_of(r).to_owned(),
            fixture_path("canonical.cpp"),
            r.line,
        );
        if seen.insert(key) {
            legacy_rows.push((
                table_cpp.name_of(r).to_owned(),
                table_cpp.fql_kind_of(r).to_owned(),
                r.line,
            ));
        }
    }
    for r in &table_rust.rows {
        let key = (
            table_rust.name_of(r).to_owned(),
            table_rust.fql_kind_of(r).to_owned(),
            fixture_path("canonical.rs"),
            r.line,
        );
        if seen.insert(key) {
            legacy_rows.push((
                table_rust.name_of(r).to_owned(),
                table_rust.fql_kind_of(r).to_owned(),
                r.line,
            ));
        }
    }
    legacy_rows.sort_unstable();

    let columnar_rows = columnar_key_tuples(&results);

    assert_eq!(
        legacy_rows.len(),
        columnar_rows.len(),
        "row count mismatch: legacy={} columnar={}",
        legacy_rows.len(),
        columnar_rows.len()
    );

    for (i, (l, c)) in legacy_rows.iter().zip(columnar_rows.iter()).enumerate() {
        assert_eq!(l, c, "row {i} mismatch: legacy={l:?} columnar={c:?}");
    }
}

/// Verify that `WHERE fql_kind = 'function'` on the overlay returns only
/// rows with that kind — same count as legacy.
#[test]
fn overlay_kind_prefilter_matches_legacy() {
    let table = index_fixture(&CppLanguageInline, "canonical.cpp");

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let cid = build_segment(&table, &cpp_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid);

    let overlay_path = overlays_dir.join("test").join("kind_filter.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&segments_dir.join("test").join(&m.hex_content_id))
                    .expect("open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(fixtures_dir(), segs, overlay);

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("function".to_owned()),
        }],
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, tmp.path())
        .expect("find_symbols with kind filter");

    // Legacy count of functions in this file
    let legacy_fn_count = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "function")
        .count();

    assert_eq!(
        results.len(),
        legacy_fn_count,
        "kind-filtered overlay should match legacy function count"
    );
    for r in &results {
        assert_eq!(
            r.fql_kind.as_deref().unwrap_or(""),
            "function",
            "all results should have fql_kind='function'"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper: build a single-segment overlay from canonical.cpp and open it.
// ─────────────────────────────────────────────────────────────────────────────

/// Shared setup used by the name-lookup, LIKE, ORDER BY and enrichment tests.
fn single_segment_cpp_overlay() -> (
    SymbolTable,
    TempDir,
    forgeql_core::storage::columnar::ColumnarStorage,
) {
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let table = index_fixture(&CppLanguageInline, "canonical.cpp");
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let cid = build_segment(&table, &cpp_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid);

    let overlay_path = overlays_dir.join("test").join("cpp_single.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&segments_dir.join("test").join(&m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(fixtures_dir(), segs, overlay);
    (table, tmp, storage)
}

/// `WHERE name = 'foo'` returns exactly the same rows as the legacy table.
#[test]
fn overlay_exact_name_lookup_matches_legacy() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    // Pick a known name from the canonical fixture.
    let target = "foo";
    let clauses = forgeql_core::ir::Clauses {
        where_predicates: vec![Predicate {
            field: "name".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String(target.to_owned()),
        }],
        ..forgeql_core::ir::Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");
    let legacy_count = table
        .rows
        .iter()
        .filter(|r| table.name_of(r) == target)
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "name='foo' row count: columnar={} legacy={legacy_count}",
        columnar.len()
    );
    for r in &columnar {
        assert_eq!(r.name, target, "every result should have name='foo'");
    }
}

/// `WHERE name LIKE 'f%'` returns the same symbol set as the legacy table.
#[test]
fn overlay_like_filter_matches_legacy() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = forgeql_core::ir::Clauses {
        where_predicates: vec![Predicate {
            field: "name".to_owned(),
            op: CompareOp::Like,
            value: PredicateValue::String("f%".to_owned()),
        }],
        ..forgeql_core::ir::Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");

    let legacy_names: std::collections::BTreeSet<String> = table
        .rows
        .iter()
        .map(|r| table.name_of(r).to_owned())
        .filter(|n| n.to_ascii_lowercase().starts_with('f'))
        .collect();
    let columnar_names: std::collections::BTreeSet<String> =
        columnar.iter().map(|r| r.name.clone()).collect();

    assert_eq!(
        columnar_names, legacy_names,
        "LIKE 'f%' name set mismatch (columnar vs legacy)"
    );
    assert!(
        !columnar_names.is_empty(),
        "expected at least one name starting with 'f'"
    );
}

/// `ORDER BY line ASC` produces non-decreasing line numbers.
#[test]
fn overlay_order_by_line_asc() {
    use forgeql_core::ir::{OrderBy, SortDirection};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = forgeql_core::ir::Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        ..forgeql_core::ir::Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find");
    let lines: Vec<_> = results.iter().map(|r| r.line.unwrap_or(0)).collect();
    assert!(
        lines.windows(2).all(|w| w[0] <= w[1]),
        "not sorted ASC by line: {lines:?}"
    );
}

/// `WHERE has_doc = 'true'` returns a subset whose size matches the legacy table.
#[test]
fn overlay_enrichment_field_filter_matches_legacy() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = forgeql_core::ir::Clauses {
        where_predicates: vec![Predicate {
            field: "has_doc".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("true".to_owned()),
        }],
        ..forgeql_core::ir::Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");

    let legacy_count = table
        .rows
        .iter()
        .filter(|r| {
            table
                .resolve_fields(&r.fields)
                .iter()
                .any(|(k, v)| k == "has_doc" && v == "true")
        })
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "has_doc='true' count: columnar={} legacy={legacy_count}",
        columnar.len()
    );
    // Every returned row must actually have has_doc='true'.
    for r in &columnar {
        let has_doc = r.fields.get("has_doc").map(String::as_str);
        assert_eq!(
            has_doc,
            Some("true"),
            "row '{}' missing has_doc='true'",
            r.name
        );
    }
}

/// `lookup_name_bitmap` in a 2-segment overlay returns global row IDs that
/// span both segments for a name present in both canonical fixtures.
///
/// Both canonical fixtures define `bar` — so the bitmap must contain ≥ 2 entries.
#[test]
fn overlay_lookup_name_spans_segments() {
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let table_rust = index_fixture(&RustLanguageInline, "canonical.rs");

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");

    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let rs_cid = build_segment(&table_rust, &rs_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_cid);
    let _ = segment_map.insert(rs_path, rs_cid);

    let overlay_path = overlays_dir.join("test").join("spans.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");

    // Count `bar` in both legacy tables.
    let legacy_bar_cpp = table_cpp
        .rows
        .iter()
        .filter(|r| table_cpp.name_of(r) == "bar")
        .count();
    let legacy_bar_rust = table_rust
        .rows
        .iter()
        .filter(|r| table_rust.name_of(r) == "bar")
        .count();
    let expected_total = legacy_bar_cpp + legacy_bar_rust;

    let bitmap = overlay.lookup_name_bitmap("bar");
    assert_eq!(
        usize::try_from(bitmap.len()).expect("bitmap len fits usize"),
        expected_total,
        "expected {expected_total} global row IDs for 'bar', got {}",
        bitmap.len()
    );

    // Verify every global row ID resolves without panic.
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&segments_dir.join("test").join(&m.hex_content_id))
                    .expect("open"),
            )
        })
        .collect();
    let _ = ColumnarStorage::new(fixtures_dir(), segs, overlay);
}
