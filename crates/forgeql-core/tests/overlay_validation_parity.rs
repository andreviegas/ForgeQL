//! Overlay/columnar parity: dirty-deletion visibility and query validation.
//!
//! A committed node deleted in the dirty overlay resolves to not-found (never a
//! phantom span), dirty deletions reflect in `SHOW outline`, and unknown WHERE
//! fields / unsortable ORDER BY are rejected with guidance while a segment-backed
//! enrichment column is accepted.

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
    let mut storage = ColumnarStorage::new_unshared(worktree.clone(), segments, overlay, registry);

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
    let mut storage = ColumnarStorage::new_unshared(worktree.clone(), segments, overlay, registry);

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
        msg.contains("Symbol-row fields"),
        "error should list the fields a symbol row does carry: {msg}"
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
        msg.contains("ORDER BY size"),
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
