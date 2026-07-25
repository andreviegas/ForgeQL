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

mod overlay_harness;

use overlay_harness::*;

// ── tests ─────────────────────────────────────────────────────────────────────

// ── Phase 06b tests ───────────────────────────────────────────────────────────

/// Verify that `ParseCache` returns the same `Arc` on a cache hit and that
/// LRU eviction drops the least-recently-used entry.
#[test]
fn parse_cache_hit_and_lru_eviction() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::parse_cache::ParseCache;

    let registry = LanguageRegistry::new(vec![
        Arc::new(CppLanguage) as Arc<dyn LanguageSupport>,
        Arc::new(RustLanguage) as Arc<dyn LanguageSupport>,
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
        Arc::new(CppLanguage) as Arc<dyn LanguageSupport>,
        Arc::new(RustLanguage) as Arc<dyn LanguageSupport>,
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

// ── FT5 gate tests ───────────────────────────────────────────────────────────

/// PhaseFT5 gate: `ColumnarStorage::index_stats()` returns `Some` and
/// `stats.rows` equals the overlay row count.
#[test]
fn ft5_columnar_index_stats_rows_match_overlay() {
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_lang_cpp::CppLanguage;
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
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
            let seg_dir = seg_path(&segments_dir, &meta.source_path, &meta.hex_content_id);
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
    use forgeql_core::ast::lang::LanguageRegistry;
    use forgeql_core::engine::ForgeQLEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_lang_cpp::CppLanguage;
    use std::collections::HashMap;
    use std::sync::Arc;

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    // Build a 1-segment overlay from canonical.cpp.
    let cpp_path = fixture_path("canonical.cpp");
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
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
            let seg_dir = seg_path(&segments_dir, &meta.source_path, &meta.hex_content_id);
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
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguage, "canonical.rs");
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
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguage, "canonical.rs");
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
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguage, "canonical.rs");
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
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguage, "canonical.rs");
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

// Regression: a committed node deleted in the dirty overlay must resolve to
// not-found, NOT a phantom inverted span. Before the fix, find_node's committed
// path fell back to the stale committed line while clamping end_line to the
// shrunken file, yielding end_line < line — the "end line < start line" zombie
// node that no mutation could touch (BUG-012).
#[test]
fn find_node_reports_not_found_for_committed_node_deleted_in_dirty() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();
    let file = worktree.join("zombie.cpp");
    // OmegaFn sits far down the file so that, once it is deleted and the file
    // shrinks, its committed line lands past EOF.
    std::fs::write(
        &file,
        "void AlphaFn() {}\n\n\n\n\n\n\n\n\nvoid OmegaFn() {}\n",
    )
    .expect("write");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table = index_at_path(&CppLanguage, &file);
    let cid = build_segment(&table, &file, seg_dir.parent().unwrap());
    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file.clone(), cid);

    let overlay_path = overlay_dir.join("zombie.bin");
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
                SegmentReader::open(&seg_path(
                    seg_dir.parent().unwrap(),
                    &meta.source_path,
                    &meta.hex_content_id,
                ))
                .expect("open seg"),
            )
        })
        .collect();
    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
    let mut storage = ColumnarStorage::new(worktree.clone(), segments, overlay, registry);

    // Capture OmegaFn's committed node_id while it still exists.
    let committed = storage
        .find_symbols(&Clauses::default(), &worktree)
        .expect("find_symbols");
    let omega_id = committed
        .iter()
        .find(|m| m.name == "OmegaFn")
        .and_then(|m| m.node_id.clone())
        .expect("OmegaFn committed node_id");

    // Delete OmegaFn and shrink the file far below its committed line, then
    // reindex so the dirty segment no longer contains it.
    std::fs::write(&file, "void AlphaFn() {}\n").expect("rewrite");
    storage
        .reindex_files(std::slice::from_ref(&file))
        .expect("reindex");

    let resolved = storage
        .find_node(&omega_id, &worktree)
        .expect("find_node must not error");
    assert!(
        resolved.is_none(),
        "deleted committed node must resolve to None, got {resolved:?}"
    );
}

// Regression: SHOW outline must reflect dirty-overlay deletions. Before the fix,
// the glob form rendered the committed segment whenever it existed and skipped
// the dirty overlay, so a deleted node stayed listed at its stale pre-edit line
// (BUG-013) — the read-side trigger that handed agents the dead node_ids that
// BUG-012 then mis-resolved.
#[test]
fn show_outline_reflects_dirty_deletions() {
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::workspace::Workspace;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();
    let file = worktree.join("outline_dirty.cpp");
    std::fs::write(
        &file,
        "void AlphaFn() {}\nvoid BetaFn() {}\nvoid GammaFn() {}\n",
    )
    .expect("write");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table = index_at_path(&CppLanguage, &file);
    let cid = build_segment(&table, &file, seg_dir.parent().unwrap());
    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file.clone(), cid);

    let overlay_path = overlay_dir.join("outline_dirty.bin");
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
                SegmentReader::open(&seg_path(
                    seg_dir.parent().unwrap(),
                    &meta.source_path,
                    &meta.hex_content_id,
                ))
                .expect("open seg"),
            )
        })
        .collect();
    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
    let mut storage = ColumnarStorage::new(worktree.clone(), segments, overlay, registry);

    // Delete BetaFn and reindex so the file gains a dirty segment.
    std::fs::write(&file, "void AlphaFn() {}\nvoid GammaFn() {}\n").expect("rewrite");
    storage
        .reindex_files(std::slice::from_ref(&file))
        .expect("reindex");

    let workspace = Workspace::new(worktree).expect("workspace");
    let json = storage
        .show_outline_for_file(&workspace, "outline_dirty.cpp", true)
        .expect("show_outline");
    let names: Vec<String> = json["results"]
        .as_array()
        .expect("results array")
        .iter()
        .map(|r| r["name"].as_str().unwrap_or("").to_owned())
        .collect();

    assert!(
        !names.iter().any(|n| n == "BetaFn"),
        "SHOW outline must not list the deleted BetaFn; got {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "AlphaFn") && names.iter().any(|n| n == "GammaFn"),
        "SHOW outline must list the surviving functions; got {names:?}"
    );
}

/// A WHERE field that is neither a core field nor an enrichment column of
/// any segment is rejected upfront with guidance — never silently scanned.
#[test]
fn unknown_where_field_is_rejected_with_guidance() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = forgeql_core::ir::Clauses {
        where_predicates: vec![Predicate {
            field: "fql_grep".to_owned(),
            op: CompareOp::Matches,
            value: PredicateValue::String("anything".to_owned()),
        }],
        ..forgeql_core::ir::Clauses::default()
    };

    let err = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect_err("unknown WHERE field must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown WHERE field 'fql_grep'"),
        "error should name the field: {msg}"
    );
    assert!(
        msg.contains("Core fields"),
        "error should list core fields: {msg}"
    );
}

/// An ORDER BY field with no per-symbol value must be rejected, not ignored.
///
/// `size` is a FIND-files concept; on a symbol it resolves to nothing, so the
/// old comparator silently fell back to name order and returned alphabetical
/// rows under a `size` header.  A real enrichment metric (`lines`) must still
/// order without complaint — the guard rejects only unsortable fields.
#[test]
fn order_by_unsortable_field_is_rejected() {
    use forgeql_core::ir::{OrderBy, SortDirection};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let size_clauses = forgeql_core::ir::Clauses {
        order_by: Some(OrderBy {
            field: "size".to_owned(),
            direction: SortDirection::Desc,
        }),
        ..forgeql_core::ir::Clauses::default()
    };
    let err = storage
        .find_symbols(&size_clauses, std::path::Path::new("."))
        .expect_err("ORDER BY size on symbols must be rejected");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown ORDER BY field 'size'"),
        "error should name the field: {msg}"
    );
    assert!(
        msg.contains("FIND files"),
        "error should redirect size/depth to FIND files: {msg}"
    );

    // A genuine enrichment metric still orders fine — no over-rejection.
    let lines_clauses = forgeql_core::ir::Clauses {
        order_by: Some(OrderBy {
            field: "lines".to_owned(),
            direction: SortDirection::Desc,
        }),
        ..forgeql_core::ir::Clauses::default()
    };
    let _rows = storage
        .find_symbols(&lines_clauses, std::path::Path::new("."))
        .expect("ORDER BY lines is a valid enrichment ordering");
}

/// `naming` is written by the universal naming enricher but is absent from
/// the static field→kind map — it must be accepted because the segments
/// store it as an enrichment column.
#[test]
fn segment_backed_enrichment_column_is_accepted() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = forgeql_core::ir::Clauses {
        where_predicates: vec![Predicate {
            field: "naming".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("snake_case".to_owned()),
        }],
        ..forgeql_core::ir::Clauses::default()
    };

    let result = storage.find_symbols(&clauses, std::path::Path::new("."));
    assert!(
        result.is_ok(),
        "segment-backed enrichment column must not be rejected: {:?}",
        result.err()
    );
}
