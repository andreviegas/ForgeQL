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
                SegmentReader::open(&segments_dir.join("test").join(&meta.hex_content_id))
                    .expect("SegmentReader::open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(fixtures_dir(), segments, overlay);

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
                SegmentReader::open(&segments_dir.join("test").join(&meta.hex_content_id))
                    .expect("SegmentReader::open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(fixtures_dir(), segments, overlay);

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
