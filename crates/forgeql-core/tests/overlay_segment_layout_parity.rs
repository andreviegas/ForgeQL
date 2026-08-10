//! Overlay/columnar parity: segment layout and index stats.
//!
//! Committed segments are ordered by path with contiguous, non-overlapping row
//! ranges; `index_stats` matches the overlay row count; and a session exposes a
//! columnar backend after install.

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
    let storage =
        ColumnarStorage::new_unshared(tmp.path().to_path_buf(), segments, overlay, registry);

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
    let storage = ColumnarStorage::new_unshared(
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
