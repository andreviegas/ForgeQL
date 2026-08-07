//! Overlay/columnar parity: dirty (uncommitted) overlay.
//!
//! A `DirtyOverlay` must shadow committed segment rows with in-session edits
//! and union in newly-added rows — for find_symbols, find_usages, and symbol
//! resolution — and a re-index must refresh it against the persistent segments.

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

// ─────────────────────────────────────────────────────────────────────────────
// PhaseFT1 gate tests — DirtyOverlay shadowing + union
// ─────────────────────────────────────────────────────────────────────────────

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
        let _ = builder.emit_row(SymbolRow {
            name: "SymbolA",
            fql_kind: "function",
            language: "cpp",
            line: 10,
            byte_start: 0,
            byte_end: 20,
            usages_count: 0,
        });
        let _ = builder.emit_row(SymbolRow {
            name: "SymbolB",
            fql_kind: "function",
            language: "cpp",
            line: 20,
            byte_start: 0,
            byte_end: 40,
            usages_count: 0,
        });
        builder
            .flush(
                &seg_dir.join(forgeql_core::storage::columnar::segment_rel_path(
                    std::path::Path::new("file1.cpp"),
                    &file1_hex,
                )),
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
        let _ = builder.emit_row(SymbolRow {
            name: "SymbolC",
            fql_kind: "function",
            language: "rust",
            line: 5,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        builder
            .flush(
                &seg_dir.join(forgeql_core::storage::columnar::segment_rel_path(
                    std::path::Path::new("file2.rs"),
                    &file2_hex,
                )),
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
                SegmentReader::open(&seg_dir.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &meta.source_path,
                        &meta.hex_content_id,
                    ),
                ))
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
        let _ = builder.emit_row(SymbolRow {
            name: "SymbolA",
            fql_kind: "function",
            language: "cpp",
            line: 1,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        // BUG-006 U2: find_usages reads usage POSTINGS, not definition rows —
        // give SymbolA a usage site so the shadow assertion is meaningful.
        builder.add_usage("SymbolA", 3);
        builder
            .flush(
                &seg_dir.join(forgeql_core::storage::columnar::segment_rel_path(
                    std::path::Path::new("file1.cpp"),
                    &file1_hex,
                )),
            )
            .expect("flush");
    }

    let root = tmp.path().to_path_buf();
    // The file the segments describe, as it stands after the dirty edit:
    // `SymbolB` on line 1, no `SymbolA` anywhere. `find_usages` reads the
    // workspace and lets the bytes arbitrate what the postings claim, so a
    // segment whose source file is not on disk describes bytes that are gone
    // and reports nothing — a real session cannot reach that state, and
    // leaving the fixture there would have been testing an impossible one.
    std::fs::write(root.join("file1.cpp"), "void SymbolB() {}\n").expect("worktree file");
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
                SegmentReader::open(&seg_dir.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &meta.source_path,
                        &meta.hex_content_id,
                    ),
                ))
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
    let dirty_reader =
        build_dirty_segment_with_usages(&[("SymbolB", "function", 1)], &dirty_cid, &dirty_dir);
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
        usages_a.0.is_empty(),
        "SymbolA must be shadowed after dirty overlay; got: {usages_a:?}"
    );

    // find_usages("SymbolB") must return 1 row from dirty segment.
    let usages_b = storage
        .find_usages("SymbolB", &clauses, &root)
        .expect("usages_b");
    assert_eq!(
        usages_b.0.len(),
        1,
        "SymbolB must appear in dirty segment; got: {usages_b:?}"
    );
}

/// Gate: resolve_symbol returns the dirty row (not the shadowed persistent one)
/// and returns None for a name that no longer exists in the dirty overlay.
#[test]
#[allow(clippy::too_many_lines)]
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
        let _ = builder.emit_row(SymbolRow {
            name: "SymbolA",
            fql_kind: "function",
            language: "cpp",
            line: 10,
            byte_start: 0,
            byte_end: 20,
            usages_count: 0,
        });
        let _ = builder.emit_row(SymbolRow {
            name: "SymbolB",
            fql_kind: "function",
            language: "cpp",
            line: 20,
            byte_start: 0,
            byte_end: 40,
            usages_count: 0,
        });
        builder
            .flush(
                &seg_dir.join(forgeql_core::storage::columnar::segment_rel_path(
                    std::path::Path::new("file1.cpp"),
                    &file1_hex,
                )),
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
                SegmentReader::open(&seg_dir.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &meta.source_path,
                        &meta.hex_content_id,
                    ),
                ))
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

/// Regression: `resolve_impl` Stage 1 must apply `in_glob` path filter to dirty
/// segments.  Without the fix, `SHOW body OF 'open'` with `IN 'a.rs'` would
/// return a match from `b.rs` when both are in the dirty overlay.
#[test]
#[allow(clippy::too_many_lines)]
fn dirty_overlay_resolve_respects_in_glob_filter() {
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

    let root = tmp.path().to_path_buf();

    // Persistent: one file with a unique symbol so the overlay file is created.
    let bg_cid: Vec<u8> = vec![0x77u8; 8];
    let bg_hex = bg_cid.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    {
        let mut builder = SegmentBuilder::new("test", &bg_cid);
        let _ = builder.emit_row(SymbolRow {
            name: "BgSymbol",
            fql_kind: "function",
            language: "rust",
            line: 1,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        builder
            .flush(
                &seg_dir.join(forgeql_core::storage::columnar::segment_rel_path(
                    std::path::Path::new("other.rs"),
                    &bg_hex,
                )),
            )
            .expect("bg flush");
    }
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(root.join("other.rs"), bg_cid);
    let overlay_path = overlay_dir.join("glob_filter.bin");
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
                SegmentReader::open(&seg_dir.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &meta.source_path,
                        &meta.hex_content_id,
                    ),
                ))
                .expect("open bg seg"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new(
        root.clone(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // Dirty: two files both define `open`, at different lines.
    let cid_a: Vec<u8> = vec![0xAAu8; 8];
    let cid_b: Vec<u8> = vec![0xBBu8; 8];
    let dir_a = tmp.path().join("staging").join("a");
    let dir_b = tmp.path().join("staging").join("b");
    let reader_a = build_dirty_segment(&[("open", "function", 10)], &cid_a, &dir_a);
    let reader_b = build_dirty_segment(&[("open", "function", 99)], &cid_b, &dir_b);

    // Add b first so insertion order would make b win without the fix.
    storage.dirty_mut().add_segment(
        Arc::new(reader_b),
        std::path::PathBuf::from("b.rs"),
        String::new(),
    );
    storage.dirty_mut().add_segment(
        Arc::new(reader_a),
        std::path::PathBuf::from("a.rs"),
        String::new(),
    );

    // Without `IN` filter: both files match — alphabetically-last path (`b.rs`) wins.
    let clauses_no_filter = Clauses::default();
    let loc_any = storage
        .resolve_symbol("open", &clauses_no_filter, &root)
        .unwrap();
    assert!(loc_any.is_some(), "open must resolve without filter");
    assert_eq!(
        loc_any.as_ref().unwrap().line,
        99,
        "without IN filter: alphabetically-last path (b.rs, line 99) must win; got {loc_any:?}"
    );

    // With `IN 'a.rs'` filter: only `a.rs` segment is considered.
    let clauses_a = Clauses {
        in_glob: Some("a.rs".to_string()),
        ..Clauses::default()
    };
    let loc_a = storage.resolve_symbol("open", &clauses_a, &root).unwrap();
    assert!(loc_a.is_some(), "open must resolve IN 'a.rs'");
    assert_eq!(
        loc_a.as_ref().unwrap().line,
        10,
        "IN 'a.rs' must restrict to a.rs (line 10); got {loc_a:?}"
    );

    // With `IN 'b.rs'` filter: only `b.rs` segment is considered.
    let clauses_b = Clauses {
        in_glob: Some("b.rs".to_string()),
        ..Clauses::default()
    };
    let loc_b = storage.resolve_symbol("open", &clauses_b, &root).unwrap();
    assert!(loc_b.is_some(), "open must resolve IN 'b.rs'");
    assert_eq!(
        loc_b.as_ref().unwrap().line,
        99,
        "IN 'b.rs' must restrict to b.rs (line 99); got {loc_b:?}"
    );
}

/// Regression: `resolve_impl` Stage 1 tie-breaking must be alphabetical by path,
/// not insertion-order.  Without the fix, mutating `b.rs` last made `SHOW body OF
/// 'open'` return `b.rs:open` even when `a.rs:open` is the only dirty match.
#[test]
#[allow(clippy::too_many_lines)]
fn dirty_overlay_resolve_uses_alphabetical_not_insertion_order() {
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

    let root = tmp.path().to_path_buf();

    // Persistent: one file with a unique symbol so the overlay file is created.
    let bg_cid: Vec<u8> = vec![0x55u8; 8];
    let bg_hex = bg_cid.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    {
        let mut builder = SegmentBuilder::new("test", &bg_cid);
        let _ = builder.emit_row(SymbolRow {
            name: "BgSymbol2",
            fql_kind: "function",
            language: "rust",
            line: 1,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        builder
            .flush(
                &seg_dir.join(forgeql_core::storage::columnar::segment_rel_path(
                    std::path::Path::new("other2.rs"),
                    &bg_hex,
                )),
            )
            .expect("bg flush");
    }
    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(root.join("other2.rs"), bg_cid);
    let overlay_path = overlay_dir.join("alpha_order.bin");
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
                SegmentReader::open(&seg_dir.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &meta.source_path,
                        &meta.hex_content_id,
                    ),
                ))
                .expect("open bg seg"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new(
        root.clone(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // Three dirty segments all defining `common_fn`, at different lines.
    // Added in reverse-alphabetical order to verify sort overrides insertion order.
    let cid_z: Vec<u8> = vec![0x11u8; 8];
    let cid_m: Vec<u8> = vec![0x22u8; 8];
    let cid_a: Vec<u8> = vec![0x33u8; 8];
    let dir_z = tmp.path().join("staging").join("z");
    let dir_m = tmp.path().join("staging").join("m");
    let dir_a = tmp.path().join("staging").join("a2");
    let reader_z = build_dirty_segment(&[("common_fn", "function", 300)], &cid_z, &dir_z);
    let reader_m = build_dirty_segment(&[("common_fn", "function", 200)], &cid_m, &dir_m);
    let reader_a = build_dirty_segment(&[("common_fn", "function", 100)], &cid_a, &dir_a);
    // Insertion order: z (300), m (200), a (100) — reverse alphabetical.
    // Insertion-order `.pop()` would return `a.rs` (last inserted = line 100).
    // Alphabetical `.pop()` must return `z.rs` (alphabetically last = line 300).
    storage.dirty_mut().add_segment(
        Arc::new(reader_z),
        std::path::PathBuf::from("z.rs"),
        String::new(),
    );
    storage.dirty_mut().add_segment(
        Arc::new(reader_m),
        std::path::PathBuf::from("m.rs"),
        String::new(),
    );
    storage.dirty_mut().add_segment(
        Arc::new(reader_a),
        std::path::PathBuf::from("a.rs"),
        String::new(),
    );

    let clauses = Clauses::default();
    let loc = storage
        .resolve_symbol("common_fn", &clauses, &root)
        .unwrap();
    assert!(loc.is_some(), "common_fn must resolve");
    assert_eq!(
        loc.as_ref().unwrap().line,
        300,
        "alphabetically-last path (z.rs, line 300) must win regardless of insertion order; got {loc:?}"
    );
}

// ── PhaseFT2 gate tests ────────────────────────────────────────────────────────

/// `reindex_files` on `ColumnarStorage` must:
/// 1. Shadow the persistent segment for the changed file.
/// 2. Build and register a new dirty segment from the new content.
/// 3. Leave unchanged files' symbols unaffected.
#[test]
fn reindex_updates_dirty_overlay() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_lang_cpp::CppLanguage;
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

    let table1 = index_at_path(&CppLanguage, &file1);
    let table2 = index_at_path(&CppLanguage, &file2);
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
                SegmentReader::open(&seg_dir.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &meta.source_path,
                        &meta.hex_content_id,
                    ),
                ))
                .expect("open seg"),
            )
        })
        .collect();

    let registry = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
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
