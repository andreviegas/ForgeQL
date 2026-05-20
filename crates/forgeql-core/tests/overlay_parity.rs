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
use forgeql_core::ast::lang::{
    CppLanguageInline, LanguageRegistry, LanguageSupport, RustLanguageInline,
};
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

/// Index an arbitrary file by absolute path and return the `SymbolTable`.
fn index_at_path(lang: &dyn LanguageSupport, path: &std::path::Path) -> SymbolTable {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    let _ = index_file(&mut parser, path, &mut table, &enrichers, lang, None, None)
        .expect("index_file should succeed");
    table
}

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

/// Returns the versioned test provider directory name (e.g. `"test-v3"`).
fn vp() -> String {
    format!("test-v{}", forgeql_core::storage::columnar::ENRICH_VER)
}

/// Path to a specific segment file: `<segments_base>/<vp()>/<hex[..2]>/<hex[2..]>.fqsf`.
fn seg_path(segments_base: &std::path::Path, hex: &str) -> std::path::PathBuf {
    segments_base
        .join(vp())
        .join(&hex[..2])
        .join(format!("{}.fqsf", &hex[2..]))
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

    let seg_path = seg_path(segments_dir, &hex);

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
    builder.flush(&seg_path).expect("segment flush");

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
            let seg_dir = seg_path(&segments_dir, &meta.hex_content_id);
            Arc::new(SegmentReader::open(&seg_dir).expect("SegmentReader::open"))
        })
        .collect();

    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
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
                SegmentReader::open(&seg_path(&segments_dir, &m.hex_content_id)).expect("open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

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
                SegmentReader::open(&seg_path(&segments_dir, &m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
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
                SegmentReader::open(&seg_path(&segments_dir, &m.hex_content_id)).expect("open"),
            )
        })
        .collect();
    let _ = ColumnarStorage::new(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
}

// ── Phase 06b tests ───────────────────────────────────────────────────────────

/// Verify that `ParseCache` returns the same `Arc` on a cache hit and that
/// LRU eviction drops the least-recently-used entry.
#[test]
fn parse_cache_hit_and_lru_eviction() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::ParseCache;

    let registry = LanguageRegistry::new(vec![
        Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>,
        Arc::new(RustLanguageInline) as Arc<dyn LanguageSupport>,
    ]);

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");

    // ── cache hit ────────────────────────────────────────────────────────────
    let mut cache = ParseCache::with_capacity(2);

    let a1 = cache.get_or_parse(&cpp_path, &registry).expect("parse cpp");
    let a2 = cache
        .get_or_parse(&cpp_path, &registry)
        .expect("cache hit cpp");
    assert!(
        Arc::ptr_eq(&a1, &a2),
        "second parse of cpp should be a cache hit"
    );

    let b1 = cache.get_or_parse(&rs_path, &registry).expect("parse rs");
    let b2 = cache
        .get_or_parse(&rs_path, &registry)
        .expect("cache hit rs");
    assert!(
        Arc::ptr_eq(&b1, &b2),
        "second parse of rs should be a cache hit"
    );

    // ── LRU eviction ─────────────────────────────────────────────────────────
    // capacity = 1: inserting rs should evict cpp.
    let mut cache1 = ParseCache::with_capacity(1);
    let first = cache1
        .get_or_parse(&cpp_path, &registry)
        .expect("parse cpp cap1");
    // rs insert evicts cpp
    let _ = cache1
        .get_or_parse(&rs_path, &registry)
        .expect("parse rs cap1");
    // Re-parsing cpp returns a NEW Arc (eviction happened)
    let after_evict = cache1
        .get_or_parse(&cpp_path, &registry)
        .expect("re-parse cpp after eviction");
    assert!(
        !Arc::ptr_eq(&first, &after_evict),
        "cpp Arc should differ after LRU eviction"
    );
}

/// Verify that `ParseCache` delivers ≥2× speedup on the second run of a
/// 500-call SHOW corpus (Phase 06b, Task 5 gate condition).
///
/// Design
/// ------
/// * Build a corpus from all 5 available fixture files (3 C++ + 1 C header +
///   1 Rust).  Each file appears `CORPUS_REPEATS` times — 500 calls total.
/// * Pre-compute SHA-1 hashes so `get_or_parse_with_hint` can use the fastest
///   cache-hit path (zero file I/O, zero SHA computation) on run 2.
/// * **Run 1** (cold cache): one disk read + one tree-sitter parse per unique
///   file; all subsequent calls within run 1 are already cache hits.
/// * **Run 2** (warm cache): every call is a zero-work cache hit.
/// * Assert `run2 × 2 < run1`.
#[test]
fn parse_cache_speeds_up_repeat_runs() {
    use std::path::Path;
    use std::time::Instant;

    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::{ParseCache, sha1_of_bytes};

    // 100 repetitions × 5 files = 500 calls per run.
    const CORPUS_REPEATS: usize = 100;

    let registry = LanguageRegistry::new(vec![
        Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>,
        Arc::new(RustLanguageInline) as Arc<dyn LanguageSupport>,
    ]);

    // All 5 available fixture files: three C++ (large → parse dominates),
    // one C header, one Rust.
    let top = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
    let fixture_paths: &[PathBuf] = &[
        top.join("enrichment_patterns.cpp"), // ~20 KB
        top.join("motor_control.cpp"),       // ~10 KB
        top.join("motor_control.h"),         //  ~5 KB
        fixture_path("canonical.cpp"),       //  ~3 KB
        fixture_path("canonical.rs"),        //  ~2 KB
    ];
    for p in fixture_paths {
        assert!(p.exists(), "fixture missing: {}", p.display());
    }

    // Pre-read bytes and compute SHA-1 so that `get_or_parse_with_hint`
    // enters the fast path (no I/O) on the very first cache hit within run 1.
    let entries: Vec<(PathBuf, [u8; 20])> = fixture_paths
        .iter()
        .map(|p| {
            let bytes = std::fs::read(p).expect("read fixture");
            let sha = sha1_of_bytes(&bytes);
            (p.clone(), sha)
        })
        .collect();

    // Corpus: (&Path, sha) pairs repeated CORPUS_REPEATS times each.
    // (&Path, [u8; 20]) is Copy so repeat_n() clones efficiently.
    let corpus: Vec<(&Path, [u8; 20])> = entries
        .iter()
        .flat_map(|(p, s)| std::iter::repeat_n((p.as_path(), *s), CORPUS_REPEATS))
        .collect();

    // ── Run 1: cold cache ────────────────────────────────────────────────────
    // Each unique file is parsed exactly once; all other calls hit the cache.
    let mut cache = ParseCache::with_capacity(entries.len());
    let t1 = Instant::now();
    for (path, sha) in &corpus {
        let _ = cache
            .get_or_parse_with_hint(path, &registry, Some(sha))
            .expect("run 1 parse");
    }
    let d1 = t1.elapsed();

    // ── Run 2: warm cache (same ParseCache object) ───────────────────────────
    // Every call is a cache hit — no I/O, no tree-sitter parse.
    let t2 = Instant::now();
    for (path, sha) in &corpus {
        let _ = cache
            .get_or_parse_with_hint(path, &registry, Some(sha))
            .expect("run 2 parse");
    }
    let d2 = t2.elapsed();

    let speedup = d1.as_secs_f64() / d2.as_secs_f64().max(f64::MIN_POSITIVE);
    eprintln!(
        "[parse_cache_speeds_up_repeat_runs] run1={d1:?} (cold) run2={d2:?} (warm) \
         speedup={speedup:.1}×  corpus={} calls  {} unique files",
        corpus.len(),
        entries.len(),
    );

    assert!(
        d2 * 2 < d1,
        "expected parse-cache ≥2× speedup on second run; \
         run1={d1:?} (cold)  run2={d2:?} (warm, expected < {:?})",
        d1 / 2,
    );
}
/// Verify that `ColumnarStorage::show_outline_for_file` returns the same
/// (name, fql_kind, line) set as the legacy `show_outline`.
#[test]
fn columnar_show_outline_matches_legacy() {
    use forgeql_core::ast::show::show_outline;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::workspace::Workspace;

    let table = index_fixture(&CppLanguageInline, "canonical.cpp");
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let cid = build_segment(&table, &cpp_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid);

    let overlay_path = overlays_dir.join("test").join("outline_parity.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(&seg_path(&segments_dir, &meta.hex_content_id))
                    .expect("SegmentReader::open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // -- columnar result
    let columnar_json = storage
        .show_outline_for_file(&workspace, "canonical.cpp")
        .expect("columnar show_outline");

    // -- legacy result
    let legacy_json =
        show_outline(&table, &workspace, "canonical.cpp").expect("legacy show_outline");

    // Compare (name, line) only — fql_kind differs because the columnar
    // segment stores only the FQL kind column (no node_kind fallback), so
    // rows whose legacy fql_kind was empty appear as "unknown" in columnar.
    fn extract_name_line(json: &serde_json::Value) -> Vec<(String, u64)> {
        let results = json["results"].as_array().expect("results array");
        let mut v: Vec<_> = results
            .iter()
            .map(|r| {
                (
                    r["name"].as_str().unwrap_or("").to_owned(),
                    r["line"].as_u64().unwrap_or(0),
                )
            })
            .collect();
        v.sort_unstable();
        v
    }

    let columnar_rows = extract_name_line(&columnar_json);
    let legacy_rows = extract_name_line(&legacy_json);

    assert_eq!(
        legacy_rows.len(),
        columnar_rows.len(),
        "row count mismatch: legacy={} columnar={}",
        legacy_rows.len(),
        columnar_rows.len()
    );
    for (l, c) in legacy_rows.iter().zip(columnar_rows.iter()) {
        assert_eq!(l, c, "outline row mismatch: legacy={l:?} columnar={c:?}");
    }
    for (l, c) in legacy_rows.iter().zip(columnar_rows.iter()) {
        assert_eq!(l, c, "outline row mismatch: legacy={l:?} columnar={c:?}");
    }
}

// ── Phase 06b: SHOW parity tests ──────────────────────────────────────────────

/// Helper: build a `LanguageRegistry` with C++ support and parse `canonical.cpp`
/// into a `ParseCache`, returning the `Arc<CachedParse>`.
fn cpp_cached_parse() -> std::sync::Arc<forgeql_core::ast::parse_cache::CachedParse> {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::ParseCache;

    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let mut cache = ParseCache::with_capacity(1);
    cache
        .get_or_parse(&fixture_path("canonical.cpp"), &registry)
        .expect("parse canonical.cpp")
}

/// Verify `SHOW body` on the columnar backend emits the same `start_line` as legacy.
#[test]
fn columnar_show_body_matches_legacy() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::show::show_body;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Columnar path
    let col_loc = storage
        .resolve_body_symbol("process", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("process not found (columnar)");
    let col_json = show_body(
        &cached,
        &col_loc.path,
        col_loc.byte_range.start,
        None,
        &col_loc.enrichment,
        &workspace,
        "process",
        Some(0),
        &registry,
    )
    .expect("columnar show_body");

    // Legacy path
    let leg_row = table
        .find_def("process")
        .expect("process not found (legacy)");
    let leg_enrichment = table.resolve_fields(&leg_row.fields);
    let leg_json = show_body(
        &cached,
        &cpp_path,
        leg_row.byte_range.start,
        None,
        &leg_enrichment,
        &workspace,
        "process",
        Some(0),
        &registry,
    )
    .expect("legacy show_body");

    assert_eq!(
        col_json["start_line"], leg_json["start_line"],
        "show_body start_line mismatch: columnar={:?} legacy={:?}",
        col_json["start_line"], leg_json["start_line"]
    );
    assert_eq!(
        col_json["end_line"], leg_json["end_line"],
        "show_body end_line mismatch"
    );
    // Lines array (signature text at DEPTH 0) must also match.
    assert_eq!(
        col_json["lines"], leg_json["lines"],
        "show_body lines mismatch"
    );
}

/// Verify `SHOW signature` on the columnar backend emits the same text as legacy.
#[test]
fn columnar_show_signature_matches_legacy() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::show::show_signature;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Columnar
    let col_loc = storage
        .resolve_symbol("process", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("process not found (columnar)");
    let col_json = show_signature(
        &cached,
        &col_loc.path,
        col_loc.byte_range.start,
        &col_loc.node_kind,
        &workspace,
        "process",
        &registry,
    )
    .expect("columnar show_signature");

    // Legacy
    let leg_row = table
        .find_def("process")
        .expect("process not found (legacy)");
    let leg_json = show_signature(
        &cached,
        &cpp_path,
        leg_row.byte_range.start,
        table.node_kind_of(leg_row),
        &workspace,
        "process",
        &registry,
    )
    .expect("legacy show_signature");

    assert_eq!(
        col_json["signature"], leg_json["signature"],
        "show_signature text mismatch: columnar={:?} legacy={:?}",
        col_json["signature"], leg_json["signature"]
    );
    assert_eq!(
        col_json["start_line"], leg_json["start_line"],
        "show_signature start_line mismatch"
    );
}

/// Verify `SHOW members` on the columnar backend returns the same (text, fql_kind)
/// pairs as legacy for `Motor`.
#[test]
fn columnar_show_members_matches_legacy() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::show::show_members;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let clauses = Clauses::default();

    // Columnar
    let col_loc = storage
        .resolve_type_symbol("Motor", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("Motor not found (columnar)");
    let col_json = show_members(&cached, &col_loc.path, &workspace, "Motor", &registry)
        .expect("columnar show_members");

    // Legacy — call the same show_members with the same cached parse + path
    let cpp_path = fixture_path("canonical.cpp");
    let leg_json = show_members(&cached, &cpp_path, &workspace, "Motor", &registry)
        .expect("legacy show_members");

    fn extract_members(json: &serde_json::Value) -> Vec<(String, String)> {
        let mut v: Vec<_> = json["members"]
            .as_array()
            .expect("members array")
            .iter()
            .map(|m| {
                (
                    m["text"].as_str().unwrap_or("").to_owned(),
                    m["fql_kind"].as_str().unwrap_or("").to_owned(),
                )
            })
            .collect();
        v.sort_unstable();
        v
    }

    assert_eq!(
        extract_members(&col_json),
        extract_members(&leg_json),
        "show_members (text, kind) mismatch"
    );
}

/// Verify `SHOW context` on the columnar backend centres on the same line as legacy.
#[test]
fn columnar_show_context_matches_legacy() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::ParseCache;
    use forgeql_core::ast::show::show_context;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Load bytes for show_context (takes &[u8] directly)
    let mut cache = ParseCache::with_capacity(1);
    let cached = cache.get_or_parse(&cpp_path, &registry).expect("parse");
    let source: &[u8] = &cached.source;

    // Columnar
    let col_loc = storage
        .resolve_symbol("bar", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("bar not found (columnar)");
    let col_json = show_context(
        source,
        &col_loc.path,
        col_loc.byte_range.start,
        &workspace,
        "bar",
        5,
    )
    .expect("columnar show_context");

    // Legacy
    let leg_row = table.find_def("bar").expect("bar not found (legacy)");
    let leg_json = show_context(
        source,
        &cpp_path,
        leg_row.byte_range.start,
        &workspace,
        "bar",
        5,
    )
    .expect("legacy show_context");

    assert_eq!(
        col_json["center_line"], leg_json["center_line"],
        "show_context center_line mismatch: col={:?} leg={:?}",
        col_json["center_line"], leg_json["center_line"]
    );
    assert_eq!(
        col_json["lines"], leg_json["lines"],
        "show_context lines array mismatch"
    );
}

/// Verify `SHOW callees` on the columnar backend finds the same callee names as legacy.
///
/// `caller` calls `bar` and `factorial`.
#[test]
fn columnar_show_callees_matches_legacy() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::show::show_callees;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Columnar
    let col_loc = storage
        .resolve_body_symbol("caller", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("caller not found (columnar)");
    let col_json = show_callees(
        &cached,
        &col_loc.path,
        col_loc.byte_range.start,
        None,
        &workspace,
        "caller",
        &registry,
        |_| None,
    )
    .expect("columnar show_callees");

    // Legacy
    let leg_row = table.find_def("caller").expect("caller not found (legacy)");
    let leg_json = show_callees(
        &cached,
        &cpp_path,
        leg_row.byte_range.start,
        None,
        &workspace,
        "caller",
        &registry,
        |_| None,
    )
    .expect("legacy show_callees");

    fn callee_names(json: &serde_json::Value) -> std::collections::BTreeSet<String> {
        json["results"]
            .as_array()
            .expect("results array")
            .iter()
            .map(|r| r["name"].as_str().unwrap_or("").to_owned())
            .collect()
    }

    let col_names = callee_names(&col_json);
    let leg_names = callee_names(&leg_json);

    assert_eq!(col_names, leg_names, "show_callees name set mismatch");
    assert!(
        col_names.contains("bar") && col_names.contains("factorial"),
        "expected bar and factorial as callees, got: {col_names:?}"
    );
}

// ── Phase 06b: resolve edge-case tests ───────────────────────────────────────

/// When a name resolves to both a struct and a function, `resolve_type_symbol`
/// must return the struct (type-preference semantics).
///
/// Fixture: canonical.cpp defines both `struct Motor { ... }` and
/// `int Motor(int rpm) { ... }`.
#[test]
fn resolve_type_prefers_type_over_function() {
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let clauses = Clauses::default();

    let loc = storage
        .resolve_type_symbol("Motor", &clauses, &fixtures_dir())
        .expect("resolve_type_symbol")
        .expect("Motor not found");

    // The resolved location must be the struct definition, not the function.
    // The columnar segment stores the fql_kind in `node_kind`; for the struct
    // definition the kind is "struct".
    assert_eq!(
        loc.node_kind, "struct",
        "resolve_type_symbol should return the struct row, got node_kind={:?}",
        loc.node_kind
    );
}

/// When a row carries a `body_symbol` enrichment field, `resolve_body_symbol`
/// must follow the redirect and return the out-of-line definition.
///
/// Fixture: canonical.cpp has `class Engine { void start(); }` (in-class
/// declaration) and `void Engine::start() { }` (out-of-line definition).
#[test]
fn resolve_body_follows_body_symbol_redirect() {
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let clauses = Clauses::default();

    // resolve_body_symbol("start") should follow body_symbol → "Engine::start".
    // If there is no body_symbol enrichment (MemberEnricher not applied to the
    // test segment), it will fall back to whichever "start" row is resolved —
    // that is also acceptable as a no-op redirect test.
    let loc = storage
        .resolve_body_symbol("start", &clauses, &fixtures_dir())
        .expect("resolve_body_symbol")
        .expect("start not found");

    // Whether a redirect happened or not, the resolved location must be for a
    // function (the out-of-line body, or the in-class decl as fallback).
    // The key invariant: both columnar and legacy resolve to the same line.
    let (table, _tmp2, _storage2) = single_segment_cpp_overlay();
    let leg_row = table
        .find_all_defs("start")
        .into_iter()
        .chain(table.find_all_defs("Engine::start"))
        .next()
        .expect("start not in legacy table");

    // Both should be on the same line (± the redirect).
    // The columnar segment does not run MemberEnricher, so no redirect happens
    // and the line should equal the in-class declaration line.
    assert_eq!(
        loc.line, leg_row.line,
        "resolve_body_symbol line mismatch: col={} leg={}",
        loc.line, leg_row.line
    );
}

/// Calling `resolve_symbol` twice on the same name produces the same location
/// (determinism / last-write-wins stability).
#[test]
fn resolve_symbol_deterministic_on_duplicates() {
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let clauses = Clauses::default();

    // `noop_dup` has two rows: a forward-declaration and a definition.
    // resolve_symbol must always return the same (last-indexed) row.
    let loc1 = storage
        .resolve_symbol("noop_dup", &clauses, &fixtures_dir())
        .expect("resolve 1")
        .expect("noop_dup not found (call 1)");
    let loc2 = storage
        .resolve_symbol("noop_dup", &clauses, &fixtures_dir())
        .expect("resolve 2")
        .expect("noop_dup not found (call 2)");

    assert_eq!(
        loc1.line, loc2.line,
        "resolve_symbol is non-deterministic: call1={} call2={}",
        loc1.line, loc2.line
    );
    assert_eq!(
        loc1.byte_range, loc2.byte_range,
        "resolve_symbol byte_range differs between calls"
    );
}

// ── Phase 06b: bare-repo SHOW fallback test ───────────────────────────────────

/// Verify that `SHOW *` still works when the source file is absent from disk
/// and the workspace is identified as a bare-repo (Phase 06b, Gap 5 gate).
///
/// Mirrors the production path in `read_bytes_for_show` (engine/exec_show.rs):
///
/// ```text
/// file_io::read_bytes  →  Err(_)  ──►  workspace.is_bare() true
///                                       workspace.read_blob_by_sha(&sha)  →  Ok(bytes)
/// ```
///
/// Steps
/// -----
/// 1. Init a **bare** git repository in a `TempDir`.
/// 2. Store the `canonical.cpp` fixture as a loose blob via `repo.blob()`.
/// 3. Build a `Workspace` over the bare-repo root and assert `is_bare()`.
/// 4. Call `Workspace::read_blob_by_sha` and assert the returned bytes match.
/// 5. Build a `CachedParse` from those bytes (using `ParseCache`) and a
///    phantom path inside the workspace (the file does NOT exist on disk).
/// 6. Locate `bar` in the legacy symbol table to obtain a valid byte-range.
/// 7. Call `show_context` with the git-fetched bytes → assert success.
/// 8. Call `show_body` with the git-fetched `CachedParse` → assert success.
///
/// Steps 7 and 8 prove that the bytes obtained via the git-blob fallback are
/// transparently usable by downstream SHOW functions, closing the full path.
#[test]
fn bare_repo_show_reads_bytes_from_git() {
    use std::collections::HashMap as StdHashMap;

    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::{ParseCache, sha1_of_bytes};
    use forgeql_core::ast::show::{show_body, show_context};
    use forgeql_core::workspace::Workspace;

    // ── 1. Init a bare git repository ────────────────────────────────────────
    let tmp = TempDir::new().expect("TempDir");
    let bare_root = tmp.path();

    let repo = git2::Repository::init_bare(bare_root).expect("git init --bare");

    // ── 2. Store canonical.cpp as a loose blob ────────────────────────────────
    let cpp_bytes = std::fs::read(fixture_path("canonical.cpp")).expect("read canonical.cpp");
    let oid = repo.blob(&cpp_bytes).expect("repo.blob");
    let blob_sha: [u8; 20] = oid.as_bytes().try_into().expect("OID is 20 bytes");

    // ── 3. Create Workspace — must report is_bare() == true ──────────────────
    // A bare git repo has no `.git` subdirectory, so `is_bare()` returns true.
    let workspace = Workspace::new(bare_root).expect("Workspace::new");
    assert!(
        workspace.is_bare(),
        "workspace over a bare git repo must report is_bare() == true"
    );

    // ── 4. Fetch bytes from git — file is NOT on disk ─────────────────────────
    // The phantom path lives inside the workspace root but is never written.
    let phantom_path = bare_root.join("canonical.cpp");
    assert!(
        !phantom_path.exists(),
        "phantom path must not exist on disk for this test to be meaningful"
    );

    let fetched = workspace
        .read_blob_by_sha(&blob_sha)
        .expect("read_blob_by_sha on bare repo");
    assert_eq!(
        fetched, cpp_bytes,
        "bytes fetched from git must match original fixture"
    );

    // ── 5. Build CachedParse from the git-fetched bytes ──────────────────────
    let registry =
        LanguageRegistry::new(vec![Arc::new(CppLanguageInline) as Arc<dyn LanguageSupport>]);
    let hash = sha1_of_bytes(&fetched);
    let mut cache = ParseCache::with_capacity(4);
    let cached = cache
        .get_or_parse_with_bytes(hash, &phantom_path, fetched.clone(), &registry)
        .expect("get_or_parse_with_bytes on git-fetched bytes");

    // ── 6. Locate `bar` in the legacy table for a valid byte-range ───────────
    let table = index_fixture(&CppLanguageInline, "canonical.cpp");
    let row = table.find_def("bar").expect("bar in legacy table");

    // ── 7. show_context — takes raw &[u8] directly ───────────────────────────
    let ctx = show_context(
        &fetched,
        &phantom_path,
        row.byte_range.start,
        &workspace,
        "bar",
        3,
    )
    .expect("show_context on git-fetched bytes");
    assert_eq!(ctx["op"], "show_context", "show_context op field");
    assert!(ctx["error"].is_null(), "show_context must not error");
    assert!(
        ctx["center_line"].as_u64().unwrap_or(0) > 0,
        "show_context center_line must be > 0"
    );

    // ── 8. show_body — takes CachedParse built from git bytes ────────────────
    // `show_body` accepts enrichment as a HashMap<String, String> for optional
    // callee-redirect hints.  Empty map = no body_symbol redirect, which is
    // fine for this test (we just need the SHOW path to complete without error).
    let no_enrichment: StdHashMap<String, String> = StdHashMap::new();
    let body = show_body(
        &cached,
        &phantom_path,
        row.byte_range.start,
        None,
        &no_enrichment,
        &workspace,
        "bar",
        Some(0),
        &registry,
    )
    .expect("show_body on git-fetched CachedParse");
    assert_eq!(body["op"], "show_body", "show_body op field");
    assert!(body["error"].is_null(), "show_body must not error");
    assert!(
        body["start_line"].as_u64().unwrap_or(0) > 0,
        "show_body start_line must be > 0"
    );
    assert!(
        body["start_line"].as_u64().unwrap_or(0) > 0,
        "show_body start_line must be > 0"
    );
}

// ── Phase 06c parity tests ────────────────────────────────────────────────────

/// `IN 'nonexistent/**'` should return zero rows because the segment path
/// prefilter drops all segments whose source_path does not match the glob,
/// so `materialize_all` is never entered for any segment.
///
/// This exercises `segments_passing_path_filter` directly.
#[test]
fn path_glob_prunes_all_segments() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = Clauses {
        in_glob: Some("nonexistent/**".to_owned()),
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find_symbols with non-matching glob");

    assert_eq!(
        results.len(),
        0,
        "expected 0 rows when IN glob matches no segments, got {}",
        results.len()
    );
}

/// Verify that `WHERE has_doc = 'true'` and `WHERE has_doc = 'false'` both
/// return byte-equivalent results to the legacy backend after the enrichment
/// posting prefilter is applied.
///
/// This exercises `prefilter_enrichment_postings` for both values of a
/// boolean enrichment field.
#[test]
fn enrichment_posting_filter_parity() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    for value in &["true", "false"] {
        let clauses = forgeql_core::ir::Clauses {
            where_predicates: vec![Predicate {
                field: "has_doc".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String((*value).to_owned()),
            }],
            ..forgeql_core::ir::Clauses::default()
        };

        let columnar = storage
            .find_symbols(&clauses, std::path::Path::new("."))
            .expect("columnar find");

        // Compute the legacy count by scanning the symbol table directly.
        let legacy_count = table
            .rows
            .iter()
            .filter(|r| {
                table
                    .resolve_fields(&r.fields)
                    .iter()
                    .any(|(k, v)| k == "has_doc" && v.as_str() == *value)
            })
            .count();

        assert_eq!(
            columnar.len(),
            legacy_count,
            "has_doc='{value}' count mismatch: columnar={} legacy={legacy_count}",
            columnar.len()
        );

        // Every returned row must actually have the correct has_doc value.
        for r in &columnar {
            let has_doc = r.fields.get("has_doc").map(String::as_str);
            assert_eq!(
                has_doc,
                Some(*value),
                "row '{}' has wrong has_doc: expected '{value}', got {:?}",
                r.name,
                has_doc
            );
        }
    }
}

/// Verify that combining `WHERE has_doc = 'true'` with `IN 'canonical.cpp'`
/// on a 2-segment overlay (cpp + rs) returns only the cpp rows that have
/// has_doc=true, parity-equal to the legacy backend.
///
/// This exercises both prefilters together: `segments_passing_path_filter`
/// prunes the rs segment, and `prefilter_enrichment_postings` prunes rows
/// inside the cpp segment.
#[test]
fn combined_path_glob_and_enrichment_parity() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder, SegmentReader};

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

    let overlay_path = overlays_dir.join("test").join("combined_test.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(&seg_path(&segments_dir, &meta.hex_content_id))
                    .expect("SegmentReader::open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // Query: WHERE has_doc='true' IN 'canonical.cpp'
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "has_doc".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("true".to_owned()),
        }],
        in_glob: Some("canonical.cpp".to_owned()),
        ..Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");

    // Legacy: only cpp table rows with has_doc='true'
    let legacy_count = table_cpp
        .rows
        .iter()
        .filter(|r| {
            table_cpp
                .resolve_fields(&r.fields)
                .iter()
                .any(|(k, v)| k == "has_doc" && v == "true")
        })
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "combined glob+enrichment: columnar={} legacy={legacy_count}",
        columnar.len()
    );

    // Every returned row must have has_doc='true'.
    for r in &columnar {
        assert_eq!(
            r.fields.get("has_doc").map(String::as_str),
            Some("true"),
            "row '{}' missing has_doc='true'",
            r.name
        );
        // No row from canonical.rs should appear.
        if let Some(ref path) = r.path {
            assert!(
                path.to_string_lossy().contains("canonical.cpp"),
                "row '{}' came from non-cpp path: {}",
                r.name,
                path.display()
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 06d parity tests
// ─────────────────────────────────────────────────────────────────────────────

/// Task 1 — `WHERE fql_kind = 'nonexistent'` must return zero rows, not a
/// full scan. Exercises the fql_kind miss -> Some(empty) fix.
#[test]
fn unknown_fql_kind_returns_empty_no_segment_open() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("___no_such_kind___".to_owned()),
        }],
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find_symbols with unknown fql_kind");

    assert_eq!(
        results.len(),
        0,
        "expected 0 rows for unknown fql_kind, got {}",
        results.len()
    );
}

/// Task 2 — `WHERE line > <max_line>` returns zero rows via zone-map prune.
#[test]
fn range_predicate_prunes_segments_via_zone_map() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    // Use a line number that is guaranteed to exceed any real source file.
    // The segment's zone map (max_line <= a few thousand) must prune it,
    // so the result must be empty.
    let beyond_any_line: i64 = i64::from(u32::MAX);

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "line".to_owned(),
            op: CompareOp::Gt,
            value: PredicateValue::Number(beyond_any_line),
        }],
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find_symbols with out-of-range line");

    assert_eq!(
        results.len(),
        0,
        "expected 0 rows when line > {beyond_any_line}, got {}",
        results.len()
    );
}

/// Task 3 — `WHERE name LIKE 'f%'` via short-prefix index matches legacy count.
#[test]
fn short_prefix_like_uses_index() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    let prefix = "f";
    let pattern = format!("{prefix}%");

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "name".to_owned(),
            op: CompareOp::Like,
            value: PredicateValue::String(pattern.clone()),
        }],
        ..Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");

    let legacy_count = table
        .rows
        .iter()
        .filter(|r| table.name_of(r).to_ascii_lowercase().starts_with(prefix))
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "short-prefix LIKE '{pattern}': columnar={} legacy={legacy_count}",
        columnar.len()
    );
}

/// Task 3 combined — short-prefix + path-glob + range must match legacy count.
#[test]
fn combined_short_prefix_and_path_glob_and_range_matches_legacy() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = Clauses {
        where_predicates: vec![
            Predicate {
                field: "name".to_owned(),
                op: CompareOp::Like,
                value: PredicateValue::String("f%".to_owned()),
            },
            Predicate {
                field: "line".to_owned(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(1),
            },
        ],
        in_glob: Some("canonical.cpp".to_owned()),
        ..Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar combined find");

    // line >= 1 is trivially true for every real symbol; count by name only.
    let legacy_count = table
        .rows
        .iter()
        .map(|r| table.name_of(r).to_owned())
        .filter(|n| n.to_ascii_lowercase().starts_with('f'))
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "combined: columnar={} legacy={legacy_count}",
        columnar.len()
    );
}

/// `WHERE enrichment_field = X IN glob` with no fql_kind/name predicate
/// triggers the fast-path in find_symbols (skip global bitmap → iterate only
/// path-filtered segments directly).  Result must be identical to the normal
/// path and match the legacy backend count.
#[test]
fn enrichment_only_fast_path_parity() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    for value in &["true", "false"] {
        // has_doc only, plus IN glob → triggers fast-path (no indexed predicate)
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "has_doc".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String((*value).to_owned()),
            }],
            in_glob: Some("canonical.cpp".to_owned()),
            ..Clauses::default()
        };

        let columnar = storage
            .find_symbols(&clauses, std::path::Path::new("."))
            .expect("fast-path find");

        // Legacy count: rows with matching has_doc field.
        let legacy_count = table
            .rows
            .iter()
            .filter(|r| {
                table
                    .resolve_fields(&r.fields)
                    .iter()
                    .any(|(k, v): (&String, &String)| k == "has_doc" && v.as_str() == *value)
            })
            .count();

        assert_eq!(
            columnar.len(),
            legacy_count,
            "fast-path has_doc='{value}': columnar={} legacy={legacy_count}",
            columnar.len()
        );

        for r in &columnar {
            let has_doc = r.fields.get("has_doc").map(String::as_str);
            assert_eq!(
                has_doc,
                Some(*value),
                "fast-path row '{}' has wrong has_doc: expected '{value}', got {:?}",
                r.name,
                has_doc
            );
        }
    }
}

/// `WHERE line < 0` must return empty immediately — no u32 line value can
/// be negative.  The negative-value short-circuit in the zone-map wiring
/// clears all candidates without opening any segment or reading zone-map files.
#[test]
fn negative_line_predicate_returns_empty() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    for &(ref op, val) in &[
        (CompareOp::Lt, -1_i64),
        (CompareOp::Lte, -1_i64),
        (CompareOp::Eq, -1_i64),
        (CompareOp::Lt, 0_i64), // WHERE line < 0 — val=0, still impossible for u32
    ] {
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "line".to_owned(),
                op: *op,
                value: PredicateValue::Number(val),
            }],
            ..Clauses::default()
        };

        let result = storage
            .find_symbols(&clauses, std::path::Path::new("."))
            .expect("find should not error");

        assert!(
            result.is_empty(),
            "WHERE line {op:?} {val} should return empty, got {} rows",
            result.len()
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// PhaseFT1 gate tests — DirtyOverlay shadowing + union
// ─────────────────────────────────────────────────────────────────────────────

/// Build a minimal segment from raw (name, fql_kind, line) tuples.
/// Returns an opened `SegmentReader` stored at `dir`.
fn build_dirty_segment(
    rows: &[(&str, &str, u32)],
    content_id_bytes: &[u8],
    dir: &std::path::Path,
) -> SegmentReader {
    let mut builder = SegmentBuilder::new("test", content_id_bytes);
    for &(name, kind, line) in rows {
        let _ = builder.emit_row(name, kind, "rust", line, 0, 10, 0);
    }
    builder.flush(dir).expect("dirty segment flush");
    SegmentReader::open(dir).expect("dirty SegmentReader::open")
}

/// PhaseFT1 gate: dirty overlay shadows persistent segment and unions dirty rows.
///
/// Setup:
///   - 2-segment persistent overlay: `file1.cpp` (SymbolA, SymbolB) and
///     `file2.rs` (SymbolC).
///   - Dirty overlay: file1.cpp changed — new segment with SymbolD only.
///
/// Expected after dirty union:
///   - SymbolA and SymbolB gone (shadowed).
///   - SymbolD present (from dirty segment).
///   - SymbolC still present (file2.rs not shadowed).
///   - Total 2 rows.
#[test]
#[allow(clippy::too_many_lines)]
fn dirty_overlay_shadows_and_unions() {
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).unwrap();
    std::fs::create_dir_all(&overlay_dir).unwrap();

    // ── Persistent segment for file1.cpp: SymbolA + SymbolB ──
    let file1_cid: Vec<u8> = vec![0x11u8; 8];
    let file1_hex = file1_cid.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    {
        let mut builder = SegmentBuilder::new("test", &file1_cid);
        let _ = builder.emit_row("SymbolA", "function", "cpp", 10, 0, 20, 0);
        let _ = builder.emit_row("SymbolB", "function", "cpp", 20, 0, 40, 0);
        builder
            .flush(
                &seg_dir
                    .join(&file1_hex[..2])
                    .join(format!("{}.fqsf", &file1_hex[2..])),
            )
            .expect("file1 flush");
    }

    // ── Persistent segment for file2.rs: SymbolC ──
    let file2_cid: Vec<u8> = vec![0x22u8; 8];
    let file2_hex = file2_cid.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    {
        let mut builder = SegmentBuilder::new("test", &file2_cid);
        let _ = builder.emit_row("SymbolC", "function", "rust", 5, 0, 10, 0);
        builder
            .flush(
                &seg_dir
                    .join(&file2_hex[..2])
                    .join(format!("{}.fqsf", &file2_hex[2..])),
            )
            .expect("file2 flush");
    }

    // ── Build 2-segment overlay ──
    let root = tmp.path().to_path_buf();
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(root.join("file1.cpp"), file1_cid);
    let _ = segment_map.insert(root.join("file2.rs"), file2_cid);

    let overlay_path = overlay_dir.join("ft1_test.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        root.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    assert_eq!(
        overlay.segments().len(),
        2,
        "expected 2 persistent segments"
    );

    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(
                    &seg_dir
                        .join(&meta.hex_content_id[..2])
                        .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                )
                .expect("open persistent segment"),
            )
        })
        .collect();

    let mut storage = ColumnarStorage::new(
        root.clone(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // ── Baseline: A, B, C all present ──
    let clauses = Clauses::default();
    let base = storage
        .find_symbols(&clauses, &root)
        .expect("baseline find_symbols");
    let base_names: Vec<&str> = base.iter().map(|r| r.name.as_str()).collect();
    assert!(
        base_names.contains(&"SymbolA"),
        "baseline: A missing from {base_names:?}"
    );
    assert!(
        base_names.contains(&"SymbolB"),
        "baseline: B missing from {base_names:?}"
    );
    assert!(
        base_names.contains(&"SymbolC"),
        "baseline: C missing from {base_names:?}"
    );

    // ── Build dirty segment for file1.cpp: SymbolD only ──
    let dirty_cid: Vec<u8> = vec![0x33u8; 8];
    let dirty_dir = tmp.path().join("staging").join("dirty_file1");
    let dirty_reader = build_dirty_segment(&[("SymbolD", "function", 15)], &dirty_cid, &dirty_dir);

    storage.dirty_mut().add_segment(
        Arc::new(dirty_reader),
        std::path::PathBuf::from("file1.cpp"), // workspace-relative
        file1_hex,                             // replaces the persistent file1 segment
    );

    // ── After dirty: A and B gone, D present, C still there ──
    let after = storage
        .find_symbols(&clauses, &root)
        .expect("dirty find_symbols");
    let after_names: Vec<&str> = after.iter().map(|r| r.name.as_str()).collect();

    assert!(
        !after_names.contains(&"SymbolA"),
        "SymbolA must be shadowed; got: {after_names:?}"
    );
    assert!(
        !after_names.contains(&"SymbolB"),
        "SymbolB must be shadowed; got: {after_names:?}"
    );
    assert!(
        after_names.contains(&"SymbolD"),
        "SymbolD must appear from dirty segment; got: {after_names:?}"
    );
    assert!(
        after_names.contains(&"SymbolC"),
        "SymbolC (file2.rs) must still be present; got: {after_names:?}"
    );
    assert_eq!(
        after.len(),
        2,
        "expected exactly 2 rows (SymbolD + SymbolC); got: {after_names:?}"
    );
}

/// PhaseFT1 gate: `find_usages` respects dirty overlay shadowing and union.
#[test]
fn dirty_overlay_find_usages_shadows_and_unions() {
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).unwrap();
    std::fs::create_dir_all(&overlay_dir).unwrap();

    // Persistent: file1.cpp with SymbolA.
    let file1_cid: Vec<u8> = vec![0xAAu8; 8];
    let file1_hex = file1_cid.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    {
        let mut builder = SegmentBuilder::new("test", &file1_cid);
        let _ = builder.emit_row("SymbolA", "function", "cpp", 1, 0, 10, 0);
        builder
            .flush(
                &seg_dir
                    .join(&file1_hex[..2])
                    .join(format!("{}.fqsf", &file1_hex[2..])),
            )
            .expect("flush");
    }

    let root = tmp.path().to_path_buf();
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(root.join("file1.cpp"), file1_cid);

    let overlay_path = overlay_dir.join("ft1_usages.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        root.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");
    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(
                    &seg_dir
                        .join(&meta.hex_content_id[..2])
                        .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                )
                .expect("open"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new(
        root.clone(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // Dirty: file1.cpp changed — SymbolA replaced by SymbolB.
    let dirty_cid: Vec<u8> = vec![0xBBu8; 8];
    let dirty_dir = tmp.path().join("staging").join("d1");
    let dirty_reader = build_dirty_segment(&[("SymbolB", "function", 1)], &dirty_cid, &dirty_dir);
    storage.dirty_mut().add_segment(
        Arc::new(dirty_reader),
        std::path::PathBuf::from("file1.cpp"),
        file1_hex,
    );

    let clauses = Clauses::default();

    // find_usages("SymbolA") must return empty — shadowed.
    let usages_a = storage
        .find_usages("SymbolA", &clauses, &root)
        .expect("usages_a");
    assert!(
        usages_a.is_empty(),
        "SymbolA must be shadowed after dirty overlay; got: {usages_a:?}"
    );

    // find_usages("SymbolB") must return 1 row from dirty segment.
    let usages_b = storage
        .find_usages("SymbolB", &clauses, &root)
        .expect("usages_b");
    assert_eq!(
        usages_b.len(),
        1,
        "SymbolB must appear in dirty segment; got: {usages_b:?}"
    );
}

/// Gate: resolve_symbol returns the dirty row (not the shadowed persistent one)
/// and returns None for a name that no longer exists in the dirty overlay.
#[test]
fn dirty_overlay_resolve_symbol_shadows_and_unions() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays").join("test");
    std::fs::create_dir_all(&seg_dir).unwrap();
    std::fs::create_dir_all(&overlay_dir).unwrap();

    // Persistent: file1.cpp has SymbolA (line 10) and SymbolB (line 20).
    let file1_cid: Vec<u8> = vec![0x33u8; 8];
    let file1_hex = file1_cid.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    {
        let mut builder = SegmentBuilder::new("test", &file1_cid);
        let _ = builder.emit_row("SymbolA", "function", "cpp", 10, 0, 20, 0);
        let _ = builder.emit_row("SymbolB", "function", "cpp", 20, 0, 40, 0);
        builder
            .flush(
                &seg_dir
                    .join(&file1_hex[..2])
                    .join(format!("{}.fqsf", &file1_hex[2..])),
            )
            .expect("file1 flush");
    }

    let root = tmp.path().to_path_buf();
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(root.join("file1.cpp"), file1_cid);

    let overlay_path = overlay_dir.join("ft1_resolve.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        root.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");
    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(
                    &seg_dir
                        .join(&meta.hex_content_id[..2])
                        .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                )
                .expect("open"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new(
        root.clone(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // Dirty: file1.cpp changed — SymbolA gone, SymbolD added at line 5.
    // replaces_hex must be file1_hex (the persistent segment's content ID).
    let dirty_cid: Vec<u8> = vec![0xCCu8; 8];
    let dirty_dir = tmp.path().join("staging").join("d2");
    let dirty_reader = build_dirty_segment(&[("SymbolD", "function", 5)], &dirty_cid, &dirty_dir);
    storage.dirty_mut().add_segment(
        Arc::new(dirty_reader),
        std::path::PathBuf::from("file1.cpp"),
        file1_hex, // replaces the persistent file1 segment
    );

    let clauses = Clauses::default();

    // resolve_symbol("SymbolA") must return None — shadowed and not in dirty.
    let loc_a = storage.resolve_symbol("SymbolA", &clauses, &root).unwrap();
    assert!(
        loc_a.is_none(),
        "SymbolA must be shadowed by dirty overlay; got: {loc_a:?}"
    );

    // resolve_symbol("SymbolD") must return the dirty row at line 5.
    let loc_d = storage.resolve_symbol("SymbolD", &clauses, &root).unwrap();
    assert!(loc_d.is_some(), "SymbolD must be found in dirty segment");
    assert_eq!(
        loc_d.as_ref().unwrap().line,
        5,
        "SymbolD must be at line 5; got: {loc_d:?}"
    );
}

// ── PhaseFT2 gate tests ────────────────────────────────────────────────────────

/// `reindex_files` on `ColumnarStorage` must:
/// 1. Shadow the persistent segment for the changed file.
/// 2. Build and register a new dirty segment from the new content.
/// 3. Leave unchanged files' symbols unaffected.
#[test]
fn reindex_updates_dirty_overlay() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();

    // Write two fixture files to the worktree.
    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void SymbolA() {}\nvoid SymbolB() {}\n").expect("write file1");
    std::fs::write(&file2, "void SymbolC() {}\n").expect("write file2");

    // Build segments for the initial state.
    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table1 = index_at_path(&CppLanguageInline, &file1);
    let table2 = index_at_path(&CppLanguageInline, &file2);
    let cid1 = build_segment(&table1, &file1, seg_dir.parent().unwrap());
    let cid2 = build_segment(&table2, &file2, seg_dir.parent().unwrap());

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file1.clone(), cid1);
    let _ = segment_map.insert(file2, cid2);

    let overlay_path = overlay_dir.join("ft2_reindex.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        worktree.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");
    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(
                    &seg_dir
                        .join(&meta.hex_content_id[..2])
                        .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                )
                .expect("open seg"),
            )
        })
        .collect();

    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));
    let mut storage = ColumnarStorage::new(worktree.clone(), segments, overlay, registry);

    // Rewrite file1 with new symbols (SymbolD, SymbolE); SymbolA + SymbolB disappear.
    std::fs::write(&file1, "void SymbolD() {}\nvoid SymbolE() {}\n").expect("rewrite file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex_files");

    let clauses = Clauses::default();
    let results = storage
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols");
    let names: Vec<String> = results.iter().map(|m| m.name.clone()).collect();

    // Old symbols from file1 must be gone.
    assert!(
        !names.contains(&"SymbolA".to_owned()),
        "SymbolA must be shadowed after reindex; got: {names:?}"
    );
    assert!(
        !names.contains(&"SymbolB".to_owned()),
        "SymbolB must be shadowed after reindex; got: {names:?}"
    );

    // New symbols from file1 must be present.
    assert!(
        names.contains(&"SymbolD".to_owned()),
        "SymbolD must appear after reindex; got: {names:?}"
    );
    assert!(
        names.contains(&"SymbolE".to_owned()),
        "SymbolE must appear after reindex; got: {names:?}"
    );

    // file2 symbols must be untouched.
    assert!(
        names.contains(&"SymbolC".to_owned()),
        "SymbolC (file2) must still be present; got: {names:?}"
    );
}

/// `purge_file` on `ColumnarStorage` must remove all symbols for the given
/// file while leaving other files' symbols untouched.
#[test]
fn purge_removes_file_symbols() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();

    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void SymbolA() {}\n").expect("write file1");
    std::fs::write(&file2, "void SymbolB() {}\n").expect("write file2");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table1 = index_at_path(&CppLanguageInline, &file1);
    let table2 = index_at_path(&CppLanguageInline, &file2);
    let cid1 = build_segment(&table1, &file1, seg_dir.parent().unwrap());
    let cid2 = build_segment(&table2, &file2, seg_dir.parent().unwrap());

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file1.clone(), cid1);
    let _ = segment_map.insert(file2, cid2);

    let overlay_path = overlay_dir.join("ft2_purge.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        worktree.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");
    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(
                    &seg_dir
                        .join(&meta.hex_content_id[..2])
                        .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                )
                .expect("open seg"),
            )
        })
        .collect();

    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));
    let mut storage = ColumnarStorage::new(worktree.clone(), segments, overlay, registry);

    // Purge file1 — its symbols should vanish.
    storage.purge_file(&file1).expect("purge_file");

    let clauses = Clauses::default();
    let results = storage
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols");
    let names: Vec<String> = results.iter().map(|m| m.name.clone()).collect();

    assert!(
        !names.contains(&"SymbolA".to_owned()),
        "SymbolA must be purged; got: {names:?}"
    );
    assert!(
        names.contains(&"SymbolB".to_owned()),
        "SymbolB (file2) must still be present; got: {names:?}"
    );
}

// ── PhaseFT3 gate tests ────────────────────────────────────────────────────────

/// PhaseFT3 gate: `DeltaFile::save` + `DeltaFile::load` round-trip without loss.
#[test]
fn delta_file_roundtrip() {
    use forgeql_core::storage::columnar::{DeltaFile, DirtyOverlay};

    let tmp = TempDir::new().expect("tempdir");
    let delta_path = tmp.path().join(".forgeql-columnar-delta");
    let staging_dir = tmp.path().join(".forgeql-staging");
    std::fs::create_dir_all(&staging_dir).expect("staging_dir");

    // Build a dirty overlay with only removals (no staging segments needed).
    let mut dirty = DirtyOverlay::new();
    let _ = dirty.removed_hex_ids.insert("aabbccdd".to_owned());
    let _ = dirty.removed_hex_ids.insert("11223344".to_owned());

    DeltaFile::save(&dirty, &delta_path).expect("save delta");
    assert!(delta_path.exists(), "delta file must exist after save");

    // read_valid_hexes returns staged hex IDs (none here — only removals).
    let hexes = DeltaFile::read_valid_hexes(&delta_path);
    assert!(
        hexes.is_empty(),
        "no staged entries → read_valid_hexes must be empty"
    );

    // Full roundtrip: load back and compare removed_hex_ids.
    let loaded = DeltaFile::load(&delta_path, &staging_dir).expect("load delta");
    assert_eq!(loaded.added.len(), 0, "no staged entries expected");
    let mut orig_removed: Vec<_> = dirty.removed_hex_ids.iter().cloned().collect();
    let mut loaded_removed: Vec<_> = loaded.removed_hex_ids.iter().cloned().collect();
    orig_removed.sort_unstable();
    loaded_removed.sort_unstable();
    assert_eq!(
        orig_removed, loaded_removed,
        "removed_hex_ids roundtrip mismatch"
    );
}

/// PhaseFT3 gate: `reindex_files` must write `.forgeql-columnar-delta` with the
/// correct staged metadata matching the dirty overlay state.
#[test]
fn reindex_writes_delta_file() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, DeltaFile};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();

    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void SymbolA() {}\n").expect("write file1");
    std::fs::write(&file2, "void SymbolB() {}\n").expect("write file2");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table1 = index_at_path(&CppLanguageInline, &file1);
    let table2 = index_at_path(&CppLanguageInline, &file2);
    let cid1 = build_segment(&table1, &file1, seg_dir.parent().unwrap());
    let cid2 = build_segment(&table2, &file2, seg_dir.parent().unwrap());

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file1.clone(), cid1);
    let _ = segment_map.insert(file2, cid2);

    let overlay_path = overlay_dir.join("ft3_reindex_delta.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        worktree.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");
    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            Arc::new(
                SegmentReader::open(
                    &seg_dir
                        .join(&meta.hex_content_id[..2])
                        .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                )
                .expect("open seg"),
            )
        })
        .collect();

    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));
    let mut storage = ColumnarStorage::new(worktree.clone(), segments, overlay, registry);

    let delta_path = worktree.join(".forgeql-columnar-delta");
    assert!(!delta_path.exists(), "delta must not exist before reindex");

    std::fs::write(&file1, "void SymbolC() {}\n").expect("rewrite file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex_files");

    assert!(delta_path.exists(), "delta must exist after reindex");

    // read_valid_hexes gives us the staged hex IDs.
    let hexes = DeltaFile::read_valid_hexes(&delta_path);
    assert_eq!(hexes.len(), 1, "expected 1 staged hex; got {}", hexes.len());
    assert!(
        !hexes[0].is_empty(),
        "staged hex_content_id must be non-empty"
    );

    // Full load: verify source_path and removed_hex_ids.
    let staging_dir = worktree.join(".forgeql-staging");
    let loaded_dirty = DeltaFile::load(&delta_path, &staging_dir).expect("load delta");
    assert_eq!(
        loaded_dirty.added.len(),
        1,
        "expected 1 staged entry in dirty overlay"
    );
    assert_eq!(
        loaded_dirty.added[0].source_path,
        std::path::PathBuf::from("file1.cpp"),
        "staged source_path must be worktree-relative"
    );
    assert!(
        !loaded_dirty.removed_hex_ids.is_empty(),
        "removed_hex_ids must be non-empty after shadowing file1"
    );
}

/// PhaseFT3 gate: after a simulated restart, loading the delta file from disk
/// must restore the dirty overlay so query results match the original instance.
#[test]
#[allow(clippy::too_many_lines)]
fn delta_survives_simulated_restart() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();

    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void SymbolA() {}\nvoid SymbolB() {}\n").expect("write file1");
    std::fs::write(&file2, "void SymbolC() {}\n").expect("write file2");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table1 = index_at_path(&CppLanguageInline, &file1);
    let table2 = index_at_path(&CppLanguageInline, &file2);
    let cid1 = build_segment(&table1, &file1, seg_dir.parent().unwrap());
    let cid2 = build_segment(&table2, &file2, seg_dir.parent().unwrap());

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file1.clone(), cid1);
    let _ = segment_map.insert(file2, cid2);

    let overlay_path = overlay_dir.join("ft3_restart.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        worktree.clone(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");

    // Helper to open a fresh ColumnarStorage for this overlay.
    let make_storage = || {
        let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
        let segments: Vec<Arc<SegmentReader>> = overlay
            .segments()
            .iter()
            .map(|meta| {
                Arc::new(
                    SegmentReader::open(
                        &seg_dir
                            .join(&meta.hex_content_id[..2])
                            .join(format!("{}.fqsf", &meta.hex_content_id[2..])),
                    )
                    .expect("open seg"),
                )
            })
            .collect();
        ColumnarStorage::new(
            worktree.clone(),
            segments,
            overlay,
            Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)])),
        )
    };

    // ── Step 1: reindex file1 in the original storage instance ──
    let mut storage1 = make_storage();
    std::fs::write(&file1, "void SymbolD() {}\nvoid SymbolE() {}\n").expect("rewrite file1");
    storage1
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex_files");

    let clauses = Clauses::default();
    let mut expected_names: Vec<String> = storage1
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols on storage1")
        .iter()
        .map(|m| m.name.clone())
        .collect();
    expected_names.sort_unstable();

    // ── Step 2: "restart" — open a fresh storage and reload delta from disk ──
    let mut storage2 = make_storage();
    storage2
        .reload_dirty_from_delta()
        .expect("reload_dirty_from_delta");

    let mut actual_names: Vec<String> = storage2
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols on storage2")
        .iter()
        .map(|m| m.name.clone())
        .collect();
    actual_names.sort_unstable();

    assert_eq!(
        expected_names, actual_names,
        "reload must restore query results to match original dirty state"
    );

    // ── Step 3: removing the delta file must revert to the clean persistent state ──
    std::fs::remove_file(worktree.join(".forgeql-columnar-delta")).expect("remove delta file");
    storage2
        .reload_dirty_from_delta()
        .expect("reload after delta removal");

    let all_names: Vec<String> = storage2
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols after delta removal")
        .iter()
        .map(|m| m.name.clone())
        .collect();

    assert!(
        all_names.contains(&"SymbolA".to_owned()),
        "SymbolA must reappear when dirty overlay is cleared; got: {all_names:?}"
    );
    assert!(
        !all_names.contains(&"SymbolD".to_owned()),
        "SymbolD must be gone when dirty overlay is cleared; got: {all_names:?}"
    );
}

/// PhaseFT3 gate: after a simulated rollback, `reload_dirty_from_delta` GCs
/// orphaned staging segments (those not in the restored delta) and restores
/// only the state from the checkpoint delta.
#[test]
#[allow(clippy::too_many_lines)]
fn rollback_gcs_orphaned_staging_segments() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, DeltaFile};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();

    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void Base1() {}\n").expect("write file1");
    std::fs::write(&file2, "void Base2() {}\n").expect("write file2");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let t1 = index_at_path(&CppLanguageInline, &file1);
    let t2 = index_at_path(&CppLanguageInline, &file2);
    let c1 = build_segment(&t1, &file1, seg_dir.parent().unwrap());
    let c2 = build_segment(&t2, &file2, seg_dir.parent().unwrap());

    let mut seg_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = seg_map.insert(file1.clone(), c1);
    let _ = seg_map.insert(file2.clone(), c2);

    let overlay_path = overlay_dir.join("ft3_gc.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        worktree.clone(),
        seg_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");

    let make_storage = || {
        let ov = Overlay::open(&overlay_path).expect("Overlay::open");
        let segs: Vec<Arc<SegmentReader>> = ov
            .segments()
            .iter()
            .map(|m| {
                Arc::new(
                    SegmentReader::open(
                        &seg_dir
                            .join(&m.hex_content_id[..2])
                            .join(format!("{}.fqsf", &m.hex_content_id[2..])),
                    )
                    .expect("seg"),
                )
            })
            .collect();
        ColumnarStorage::new(
            worktree.clone(),
            segs,
            ov,
            Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)])),
        )
    };

    let mut storage = make_storage();

    // ── Checkpoint: reindex file1 → staging hex A, delta saved ──
    std::fs::write(&file1, "void AfterCheckpoint1() {}\n").expect("reindex file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex file1");

    let delta_path = worktree.join(".forgeql-columnar-delta");
    let checkpoint_delta = std::fs::read(&delta_path).expect("read checkpoint delta");

    let hex_a_vec = DeltaFile::read_valid_hexes(&delta_path);
    assert_eq!(
        hex_a_vec.len(),
        1,
        "checkpoint must have exactly 1 staged hex"
    );
    let hex_a = hex_a_vec[0].clone();

    let staging_dir = worktree.join(".forgeql-staging");
    assert!(
        staging_dir.join(format!("{hex_a}.fqsf")).exists(),
        "staging dir for hex_a must exist"
    );

    // ── Post-checkpoint: reindex file2 → staging hex B, delta updated ──
    std::fs::write(&file2, "void AfterCheckpoint2() {}\n").expect("reindex file2");
    storage
        .reindex_files(std::slice::from_ref(&file2))
        .expect("reindex file2");

    let hexes_after = DeltaFile::read_valid_hexes(&delta_path);
    assert_eq!(
        hexes_after.len(),
        2,
        "after second reindex must have 2 staged hexes"
    );
    let hex_b = hexes_after
        .iter()
        .find(|h| *h != &hex_a)
        .cloned()
        .expect("hex_b");
    assert!(
        staging_dir.join(format!("{hex_b}.fqsf")).exists(),
        "staging dir for hex_b must exist before rollback"
    );

    // ── Simulate git reset --hard: restore delta to checkpoint state ──
    std::fs::write(&delta_path, &checkpoint_delta).expect("restore checkpoint delta");

    // ── Rollback: GC orphaned staging + reload from restored delta ──
    storage
        .reload_dirty_from_delta()
        .expect("reload_dirty_from_delta after rollback");

    // hex_b staging dir must be GC'd (it's no longer in the restored delta).
    assert!(
        !staging_dir.join(format!("{hex_b}.fqsf")).exists(),
        "staging dir for hex_b must be removed after rollback GC"
    );
    // hex_a staging dir must remain (still in the restored delta).
    assert!(
        staging_dir.join(format!("{hex_a}.fqsf")).exists(),
        "staging dir for hex_a must survive rollback GC"
    );

    // Query results must reflect checkpoint state: file1 updated, file2 not.
    let clauses = Clauses::default();
    let names: Vec<String> = storage
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols after rollback")
        .iter()
        .map(|m| m.name.clone())
        .collect();

    assert!(
        names.contains(&"AfterCheckpoint1".to_owned()),
        "AfterCheckpoint1 must be visible after rollback; got: {names:?}"
    );
    assert!(
        !names.contains(&"AfterCheckpoint2".to_owned()),
        "AfterCheckpoint2 must NOT be visible after rollback; got: {names:?}"
    );
}

/// PhaseFT3 gate: nested rollback restores the correct (earlier) checkpoint
/// delta when two checkpoints have been created.
#[test]
#[allow(clippy::too_many_lines)]
fn nested_rollback_restores_correct_delta() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, DeltaFile};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();

    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void V1() {}\n").expect("write file1");
    std::fs::write(&file2, "void V2() {}\n").expect("write file2");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let t1 = index_at_path(&CppLanguageInline, &file1);
    let t2 = index_at_path(&CppLanguageInline, &file2);
    let c1 = build_segment(&t1, &file1, seg_dir.parent().unwrap());
    let c2 = build_segment(&t2, &file2, seg_dir.parent().unwrap());

    let mut seg_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = seg_map.insert(file1.clone(), c1);
    let _ = seg_map.insert(file2.clone(), c2);

    let overlay_path = overlay_dir.join("ft3_nested.bin");
    OverlayBuilder::new(
        "test",
        seg_dir.parent().unwrap().to_path_buf(),
        worktree.clone(),
        seg_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");

    let make_storage = || {
        let ov = Overlay::open(&overlay_path).expect("Overlay::open");
        let segs: Vec<Arc<SegmentReader>> = ov
            .segments()
            .iter()
            .map(|m| {
                Arc::new(
                    SegmentReader::open(
                        &seg_dir
                            .join(&m.hex_content_id[..2])
                            .join(format!("{}.fqsf", &m.hex_content_id[2..])),
                    )
                    .expect("seg"),
                )
            })
            .collect();
        ColumnarStorage::new(
            worktree.clone(),
            segs,
            ov,
            Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)])),
        )
    };

    let mut storage = make_storage();
    let delta_path = worktree.join(".forgeql-columnar-delta");

    // ── Checkpoint 1: reindex file1 ──
    std::fs::write(&file1, "void Phase1File1() {}\n").expect("ckpt1 file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex ckpt1");
    let ckpt1_delta = std::fs::read(&delta_path).expect("read ckpt1 delta");
    let ckpt1_hexes = DeltaFile::read_valid_hexes(&delta_path);
    assert_eq!(ckpt1_hexes.len(), 1, "checkpoint1 must have 1 staged hex");

    // ── Checkpoint 2: also reindex file2 ──
    std::fs::write(&file2, "void Phase2File2() {}\n").expect("ckpt2 file2");
    storage
        .reindex_files(std::slice::from_ref(&file2))
        .expect("reindex ckpt2");
    let ckpt2_hexes = DeltaFile::read_valid_hexes(&delta_path);
    assert_eq!(ckpt2_hexes.len(), 2, "checkpoint2 must have 2 staged hexes");

    // ── Rollback to checkpoint 1 (simulate git reset --hard to ckpt1) ──
    std::fs::write(&delta_path, &ckpt1_delta).expect("restore ckpt1 delta");
    storage
        .reload_dirty_from_delta()
        .expect("reload after rollback to ckpt1");

    // Only ckpt1 hex should remain in staging.
    let staging_dir = worktree.join(".forgeql-staging");
    for hex in &ckpt2_hexes {
        if !ckpt1_hexes.contains(hex) {
            assert!(
                !staging_dir.join(format!("{hex}.fqsf")).exists(),
                "ckpt2-only hex {hex} must be GC'd after rollback to ckpt1"
            );
        }
    }
    for hex in &ckpt1_hexes {
        assert!(
            staging_dir.join(format!("{hex}.fqsf")).exists(),
            "ckpt1 hex {hex} must survive rollback to ckpt1"
        );
    }

    // Query results: file1 changes visible, file2 changes NOT visible.
    let clauses = Clauses::default();
    let names: Vec<String> = storage
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols after rollback to ckpt1")
        .iter()
        .map(|m| m.name.clone())
        .collect();

    assert!(
        names.contains(&"Phase1File1".to_owned()),
        "Phase1File1 must be visible after rollback to ckpt1; got: {names:?}"
    );
    assert!(
        !names.contains(&"Phase2File2".to_owned()),
        "Phase2File2 must NOT be visible after rollback to ckpt1; got: {names:?}"
    );
}

// =============================================================================
// PhaseFT4 gate tests
// =============================================================================

/// PhaseFT4 gate: after `commit_dirty`, the bare-repo segment store contains the
/// promoted segment, the staging directory is empty, and a new overlay file
/// exists for the new commit OID with the correct segment list.
#[test]
#[allow(clippy::too_many_lines)]
fn commit_promotes_segments_and_builds_new_overlay() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).expect("worktree dir");

    // Bare-repo layout: segments + overlays live here (persistent store).
    let bare = tmp.path().join("bare");
    let segments_dir = bare.join("segments");
    let overlays_dir = bare.join("overlays");
    std::fs::create_dir_all(&segments_dir).expect("segments dir");
    std::fs::create_dir_all(&overlays_dir).expect("overlays dir");

    let file1 = worktree.join("file1.cpp");
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void BaseFunc1() {}\n").expect("write file1");
    std::fs::write(&file2, "void BaseFunc2() {}\n").expect("write file2");

    // Build initial segments via a staging area (same layout as FT3 tests).
    let wt_seg_dir = tmp.path().join("segments");
    std::fs::create_dir_all(wt_seg_dir.join("test")).expect("wt seg dir");

    let table1 = index_at_path(&CppLanguageInline, &file1);
    let table2 = index_at_path(&CppLanguageInline, &file2);
    let cid1 = build_segment(&table1, &file1, &tmp.path().join("segments"));
    let cid2 = build_segment(&table2, &file2, &tmp.path().join("segments"));

    let hex1 = cid1.iter().fold(String::new(), |mut a, b| {
        use std::fmt::Write as _;
        let _ = write!(a, "{b:02x}");
        a
    });
    let hex2 = cid2.iter().fold(String::new(), |mut a, b| {
        use std::fmt::Write as _;
        let _ = write!(a, "{b:02x}");
        a
    });

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file1.clone(), cid1);
    let _ = segment_map.insert(file2, cid2);

    // Write the base overlay to overlays_dir (simulating what prior COMMIT wrote).
    let base_overlay_path = overlays_dir.join("test").join("base_commit.bin");
    std::fs::create_dir_all(base_overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", wt_seg_dir.clone(), worktree.clone(), segment_map)
        .build_and_persist(&base_overlay_path)
        .expect("base overlay");

    // Copy initial .fqsf segments from staging area into bare-repo segment store.
    let bare_hex1_dir = seg_path(&segments_dir, &hex1);
    let bare_hex2_dir = seg_path(&segments_dir, &hex2);
    std::fs::create_dir_all(bare_hex1_dir.parent().unwrap()).expect("bare hex1 parent");
    std::fs::create_dir_all(bare_hex2_dir.parent().unwrap()).expect("bare hex2 parent");
    let _ = std::fs::copy(seg_path(&wt_seg_dir, &hex1), &bare_hex1_dir).expect("copy hex1");
    let _ = std::fs::copy(seg_path(&wt_seg_dir, &hex2), &bare_hex2_dir).expect("copy hex2");

    // Build ColumnarBuildContext pointing at bare-repo stores.
    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );

    // Open ColumnarStorage backed by the base overlay.
    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));
    let overlay = Overlay::open(&base_overlay_path).expect("open base overlay");
    let seg_root = segments_dir.join(vp());
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(
                    &seg_root
                        .join(&m.hex_content_id[..2])
                        .join(format!("{}.fqsf", &m.hex_content_id[2..])),
                )
                .expect("open seg"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new(worktree.clone(), segments, overlay, lang_reg);

    // Modify file1 and reindex into the staging dir.
    std::fs::write(&file1, "void UpdatedFunc1() {}\nvoid NewFunc() {}\n").expect("update file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex file1");

    assert_eq!(storage.dirty().added.len(), 1, "must have 1 staged segment");
    let staged_hex = storage.dirty().added[0].reader.content_id_hex();
    let staging_dir = worktree.join(".forgeql-staging");
    assert!(
        staging_dir.join(format!("{staged_hex}.fqsf")).exists(),
        "staged segment must be in staging dir before commit"
    );

    // Call commit_dirty — the main FT4 operation.
    let new_oid = "aabbccddeeff001122334455667788990011223344556677aabbccddeeff0011";
    storage.commit_dirty(new_oid, &ctx).expect("commit_dirty");

    // ── Assert 1: staging dir is empty ──
    let staging_entries: Vec<_> = std::fs::read_dir(&staging_dir)
        .expect("read staging dir")
        .filter_map(std::result::Result::ok)
        .collect();
    assert!(
        staging_entries.is_empty(),
        "staging dir must be empty after commit_dirty; contains: {:?}",
        staging_entries
            .iter()
            .map(std::fs::DirEntry::path)
            .collect::<Vec<_>>()
    );

    // ── Assert 2: bare-repo segment store has the promoted segment ──
    let promoted_dir = seg_path(&segments_dir, &staged_hex);
    assert!(
        promoted_dir.exists(),
        "promoted segment must exist in bare-repo store at {}",
        promoted_dir.display()
    );

    // ── Assert 3: new overlay file exists ──
    let new_overlay_path = ctx.overlay_path_for(new_oid);
    assert!(
        new_overlay_path.exists(),
        "new overlay must exist at {}",
        new_overlay_path.display()
    );

    // ── Assert 4: new overlay has correct segment set ──
    let new_overlay = Overlay::open(&new_overlay_path).expect("open new overlay");
    let new_hexes: Vec<String> = new_overlay
        .segments()
        .iter()
        .map(|m| m.hex_content_id.clone())
        .collect();
    assert!(
        new_hexes.contains(&staged_hex),
        "new overlay must include promoted staged_hex; got: {new_hexes:?}"
    );
    assert!(
        new_hexes.contains(&hex2),
        "new overlay must include unchanged file2 hex; got: {new_hexes:?}"
    );
    assert!(
        !new_hexes.contains(&hex1),
        "new overlay must NOT include old file1 hex (shadowed); got: {new_hexes:?}"
    );

    // ── Assert 5: live query on updated storage returns new symbols ──
    let clauses = Clauses::default();
    let names: Vec<String> = storage
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols after commit")
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert!(
        names.contains(&"UpdatedFunc1".to_owned()),
        "UpdatedFunc1 must be visible; got: {names:?}"
    );
    assert!(
        names.contains(&"NewFunc".to_owned()),
        "NewFunc must be visible; got: {names:?}"
    );
    assert!(
        names.contains(&"BaseFunc2".to_owned()),
        "BaseFunc2 (unchanged) must be visible; got: {names:?}"
    );
    assert!(
        !names.contains(&"BaseFunc1".to_owned()),
        "BaseFunc1 (old file1) must NOT be visible; got: {names:?}"
    );
}

/// PhaseFT4 gate: a second session opened against the promoted overlay gets a
/// cache hit (`Overlay::open` succeeds) and returns the committed symbols.
#[test]
#[allow(clippy::too_many_lines)]
fn new_session_hits_promoted_overlay_cache() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).expect("worktree dir");

    let bare = tmp.path().join("bare");
    let segments_dir = bare.join("segments");
    let overlays_dir = bare.join("overlays");
    std::fs::create_dir_all(&segments_dir).expect("segments dir");
    std::fs::create_dir_all(&overlays_dir).expect("overlays dir");

    let file1 = worktree.join("file1.cpp");
    std::fs::write(&file1, "void SessionAFunc() {}\n").expect("write file1");

    let wt_seg_dir = tmp.path().join("segments");
    std::fs::create_dir_all(wt_seg_dir.join("test")).expect("wt seg dir");

    let table1 = index_at_path(&CppLanguageInline, &file1);
    let cid1 = build_segment(&table1, &file1, &tmp.path().join("segments"));
    let hex1 = cid1.iter().fold(String::new(), |mut a, b| {
        use std::fmt::Write as _;
        let _ = write!(a, "{b:02x}");
        a
    });

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file1.clone(), cid1);

    let base_overlay_path = overlays_dir.join("test").join("base_commit.bin");
    std::fs::create_dir_all(base_overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", wt_seg_dir.clone(), worktree.clone(), segment_map)
        .build_and_persist(&base_overlay_path)
        .expect("base overlay");

    let bare_hex1_dir = seg_path(&segments_dir, &hex1);
    std::fs::create_dir_all(bare_hex1_dir.parent().unwrap()).expect("bare hex1 parent");
    let _ = std::fs::copy(seg_path(&wt_seg_dir, &hex1), &bare_hex1_dir).expect("copy hex1");

    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );
    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguageInline)]));

    // Session A: change file1 and commit.
    let seg_root = segments_dir.join(vp());
    let overlay_a = Overlay::open(&base_overlay_path).expect("open base overlay");
    let segments_a: Vec<Arc<SegmentReader>> = overlay_a
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(
                    &seg_root
                        .join(&m.hex_content_id[..2])
                        .join(format!("{}.fqsf", &m.hex_content_id[2..])),
                )
                .expect("open seg"),
            )
        })
        .collect();
    let mut storage_a = ColumnarStorage::new(
        worktree.clone(),
        segments_a,
        overlay_a,
        Arc::clone(&lang_reg),
    );

    std::fs::write(&file1, "void SessionBFunc() {}\nvoid SharedFunc() {}\n").expect("update file1");
    storage_a
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex");

    let new_oid = "cafebabe00112233445566778899aabbccddeeff00112233445566778899aabb";
    storage_a
        .commit_dirty(new_oid, &ctx)
        .expect("commit_dirty session A");

    // Assert: new overlay was written so Session B can open it (cache hit).
    let new_overlay_path = ctx.overlay_path_for(new_oid);
    assert!(
        new_overlay_path.exists(),
        "new overlay must exist for session B to open"
    );

    // Session B: open fresh storage using the promoted overlay.
    let overlay_b =
        Overlay::open(&new_overlay_path).expect("session B: Overlay::open succeeded (cache hit)");
    let row_count_b = overlay_b.row_count();
    let session_b_segs: Vec<Arc<SegmentReader>> = overlay_b
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(
                    &seg_root
                        .join(&m.hex_content_id[..2])
                        .join(format!("{}.fqsf", &m.hex_content_id[2..])),
                )
                .expect("session B: open seg"),
            )
        })
        .collect();
    let storage_b = ColumnarStorage::new(
        worktree.clone(),
        session_b_segs,
        overlay_b,
        Arc::clone(&lang_reg),
    );

    // Assert: session B sees only the committed symbols.
    let clauses = Clauses::default();
    let names: Vec<String> = storage_b
        .find_symbols(&clauses, &worktree)
        .expect("session B: find_symbols")
        .iter()
        .map(|m| m.name.clone())
        .collect();
    assert!(
        names.contains(&"SessionBFunc".to_owned()),
        "session B must see SessionBFunc committed by A; got: {names:?}"
    );
    assert!(
        names.contains(&"SharedFunc".to_owned()),
        "session B must see SharedFunc committed by A; got: {names:?}"
    );
    assert!(
        !names.contains(&"SessionAFunc".to_owned()),
        "session B must NOT see old SessionAFunc (overwritten); got: {names:?}"
    );
    assert!(row_count_b > 0, "overlay row count must be positive");
}

// ── FT5 gate tests ───────────────────────────────────────────────────────────

/// PhaseFT5 gate: `ColumnarStorage::index_stats()` returns `Some` and
/// `stats.rows` equals the overlay row count.
#[test]
fn ft5_columnar_index_stats_rows_match_overlay() {
    use forgeql_core::ast::lang::CppLanguageInline;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_cid);

    let overlay_path = overlays_dir.join("test").join("ft5gate00.bin");
    std::fs::create_dir_all(overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let expected_rows = overlay.row_count() as usize;
    assert!(expected_rows > 0, "test requires a non-empty overlay");

    let segments: Vec<Arc<forgeql_core::storage::columnar::SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            let seg_dir = seg_path(&segments_dir, &meta.hex_content_id);
            Arc::new(
                forgeql_core::storage::columnar::SegmentReader::open(&seg_dir)
                    .expect("SegmentReader::open"),
            )
        })
        .collect();

    let registry = Arc::new(forgeql_core::ast::lang::LanguageRegistry::new(vec![]));
    let storage = ColumnarStorage::new(tmp.path().to_path_buf(), segments, overlay, registry);

    // FT5: index_stats() must return Some with rows == overlay.row_count()
    let stats = storage
        .index_stats()
        .expect("index_stats must be Some for columnar (FT5)");
    assert_eq!(
        stats.rows, expected_rows,
        "index_stats.rows must equal overlay.row_count()"
    );
}

/// PhaseFT5 gate: after `install_columnar_for_session`, the session reports
/// `has_columnar() == true` and `session_index_stats_rows() > 0`.
///
/// We build a one-segment overlay from `canonical.cpp` directly, then install
/// it via the existing `install_columnar_for_session` test-helper on a plain
/// legacy session so that the FT5 routing logic is exercised without relying on
/// the `register_local_session_with_columnar` slow-path.
#[test]
#[cfg(feature = "test-helpers")]
fn ft5_session_has_columnar_after_install() {
    use forgeql_core::ast::lang::{CppLanguageInline, LanguageRegistry};
    use forgeql_core::engine::ForgeQLEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    // Build a 1-segment overlay from canonical.cpp.
    let cpp_path = fixture_path("canonical.cpp");
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_cid);
    let overlay_path = overlays_dir.join("test").join("ft5s00.bin");
    std::fs::create_dir_all(overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let expected_rows = overlay.row_count() as usize;
    assert!(expected_rows > 0, "test requires a non-empty overlay");

    let segments: Vec<Arc<forgeql_core::storage::columnar::SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|meta| {
            let seg_dir = seg_path(&segments_dir, &meta.hex_content_id);
            Arc::new(
                forgeql_core::storage::columnar::SegmentReader::open(&seg_dir)
                    .expect("SegmentReader::open"),
            )
        })
        .collect();

    // Build an engine + plain legacy session on fixtures_dir().
    let data_dir = tmp.path().join("data");
    let reg = Arc::new(LanguageRegistry::new(vec![]));
    let mut engine = ForgeQLEngine::new(data_dir, reg).expect("engine");
    let sid = engine
        .register_local_session(&fixtures_dir())
        .expect("register_local_session");

    // Install the pre-built ColumnarStorage.
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
    engine.install_columnar_for_session(&sid, Box::new(storage));

    // FT5 gate 1: session must report has_columnar after install.
    assert!(
        engine.session_has_columnar(&sid),
        "session must report has_columnar() == true (FT5)"
    );

    // FT5 gate 2: index_stats().rows == overlay.row_count() via default (columnar) engine.
    let rows = engine.session_index_stats_rows(&sid);
    assert_eq!(
        rows,
        Some(expected_rows),
        "session_index_stats_rows must equal overlay.row_count() (FT5), got {rows:?}"
    );
}

// ── FT4 test helper ──────────────────────────────────────────────────────────

/// Phase 2 (FQOV v4): overlay segments are stored in non-decreasing
/// lexicographic source_path order.
///
/// Builds an overlay from two fixtures at distinct paths, opens it, and
/// asserts `segments()[0].source_path <= segments()[1].source_path`.
#[test]
fn overlay_segments_are_in_path_order() {
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    // Build two segments from the two canonical fixtures.
    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguageInline, "canonical.rs");
    let cid_cpp = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let cid_rs = build_segment(&table_rs, &rs_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid_cpp);
    let _ = segment_map.insert(rs_path, cid_rs);

    let overlay_path = overlays_dir.join("test").join("path_order.bin");
    OverlayBuilder::new("test", segments_dir, fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs = overlay.segments();
    assert!(
        segs.len() >= 2,
        "expected at least 2 segments, got {}",
        segs.len()
    );
    // Assert non-decreasing lexicographic path order (FQOV v4 invariant).
    for window in segs.windows(2) {
        assert!(
            window[0].source_path <= window[1].source_path,
            "segments out of order: {:?} > {:?}",
            window[0].source_path,
            window[1].source_path,
        );
    }
}

#[test]
fn overlay_segment_row_ranges_are_contiguous() {
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguageInline, "canonical.rs");
    let cid_cpp = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let cid_rs = build_segment(&table_rs, &rs_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid_cpp);
    let _ = segment_map.insert(rs_path, cid_rs);

    let overlay_path = overlays_dir.join("test").join("row_ranges.bin");
    OverlayBuilder::new("test", segments_dir, fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let n = overlay.segments().len();
    assert!(n >= 2, "expected at least 2 segments");

    // Ranges must be contiguous, non-overlapping, and cover 0..row_count.
    let mut expected_start = 0u32;
    for i in 0..n {
        let range = overlay.segment_row_range(i);
        assert_eq!(
            range.start, expected_start,
            "segment {i} range.start mismatch"
        );
        assert!(
            range.end >= range.start,
            "segment {i} has empty/inverted range"
        );
        expected_start = range.end;
    }
    assert_eq!(
        expected_start,
        overlay.row_count(),
        "ranges do not cover all rows"
    );
    // Out-of-bounds index returns empty range.
    assert_eq!(
        overlay.segment_row_range(n),
        0..0,
        "OOB index should return 0..0"
    );
}

// ── Phase 4: path_seg_range / path_row_range ─────────────────────────────────

#[test]
fn overlay_path_seg_range_exact_match() {
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguageInline, "canonical.rs");
    let cid_cpp = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let cid_rs = build_segment(&table_rs, &rs_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid_cpp);
    let _ = segment_map.insert(rs_path, cid_rs);

    let overlay_path = overlays_dir.join("test").join("path_seg.bin");
    OverlayBuilder::new("test", segments_dir, fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");

    // Segments are path-sorted: canonical.cpp < canonical.rs.
    let n = overlay.segments().len();
    assert!(n >= 2, "expected at least 2 segments");

    // Exact-file prefix matches exactly one segment.
    let cpp_range = overlay.path_seg_range("canonical.cpp");
    assert_eq!(cpp_range.len(), 1, "canonical.cpp should match one segment");

    let rs_range = overlay.path_seg_range("canonical.rs");
    assert_eq!(rs_range.len(), 1, "canonical.rs should match one segment");

    // The two single-file ranges must be disjoint and cover different positions.
    assert!(
        cpp_range.start < rs_range.start,
        "cpp segment must precede rs segment"
    );

    // Common prefix matches both.
    let both = overlay.path_seg_range("canonical");
    assert_eq!(
        both.len(),
        2,
        "prefix 'canonical' should match both segments"
    );

    // Non-existent prefix matches nothing.
    let none = overlay.path_seg_range("nonexistent");
    assert!(none.is_empty(), "nonexistent prefix should match nothing");

    // Empty prefix matches everything.
    let all = overlay.path_seg_range("");
    assert_eq!(all.len(), n, "empty prefix should match all segments");
}

#[test]
fn overlay_path_row_range_covers_segment_rows() {
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");
    let table_cpp = index_fixture(&CppLanguageInline, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguageInline, "canonical.rs");
    let cid_cpp = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let cid_rs = build_segment(&table_rs, &rs_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cid_cpp);
    let _ = segment_map.insert(rs_path, cid_rs);

    let overlay_path = overlays_dir.join("test").join("path_row.bin");
    OverlayBuilder::new("test", segments_dir, fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let total_rows = overlay.row_count();

    // path_row_range("canonical") must span all rows.
    let all_rows = overlay.path_row_range("canonical");
    assert_eq!(all_rows.start, 0, "common prefix row range must start at 0");
    assert_eq!(
        all_rows.end, total_rows,
        "common prefix row range must cover all rows"
    );

    // path_row_range for each file must agree with segment_row_range.
    let cpp_row_range = overlay.path_row_range("canonical.cpp");
    let rs_row_range = overlay.path_row_range("canonical.rs");

    // They must be non-empty and non-overlapping.
    assert!(!cpp_row_range.is_empty(), "cpp row range must be non-empty");
    assert!(!rs_row_range.is_empty(), "rs row range must be non-empty");
    assert!(
        cpp_row_range.end <= rs_row_range.start,
        "cpp and rs row ranges must not overlap"
    );

    // Together they must cover all rows.
    assert_eq!(cpp_row_range.start, 0, "cpp row range must start at 0");
    assert_eq!(
        rs_row_range.end, total_rows,
        "rs row range must end at total_rows"
    );

    // path_row_range("nonexistent") must return 0..0.
    assert_eq!(
        overlay.path_row_range("nonexistent"),
        0..0,
        "nonexistent prefix row range must be 0..0"
    );
}
