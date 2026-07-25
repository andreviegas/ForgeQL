//! Overlay/columnar parity: find over the dirty overlay.
//!
//! `find_symbols` regex-alternation matching and `find_node` handle resolution
//! must work against newly-created dirty rows and survive ordinal reassignment.

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

/// BUG-007: a `name MATCHES` regex with a top-level alternation (`A|B`) must
/// return rows matching EITHER branch. The columnar trigram prefilter split the
/// pattern at `|` and then *intersected* the per-branch candidate sets, so a
/// name had to contain every branch literal at once — which nothing does —
/// yielding zero results. Concatenation (`A.*B`) intersects correctly; only
/// alternation must not.
#[test]
fn find_symbols_matches_regex_alternation() {
    use forgeql_core::ir::ForgeQLIR;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();
    let file = worktree.join("alt.cpp");
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
    let _ = segment_map.insert(file, cid);

    let overlay_path = overlay_dir.join("alt.bin");
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
    let storage = ColumnarStorage::new(worktree.clone(), segments, overlay, registry);

    // Parse a real alternation query to obtain its clauses.
    let ops = forgeql_core::parser::parse("FIND symbols WHERE name MATCHES 'AlphaFn|GammaFn'")
        .expect("parse");
    let ForgeQLIR::FindSymbols { clauses, .. } = ops.into_iter().next().expect("op") else {
        panic!("expected FindSymbols");
    };

    let results = storage
        .find_symbols(&clauses, &worktree)
        .expect("find_symbols");
    let mut names: Vec<String> = results.iter().map(|m| m.name.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["AlphaFn".to_string(), "GammaFn".to_string()],
        "MATCHES alternation must return rows matching EITHER branch; got {names:?}"
    );
}

/// BUG-008: a node created in this session (its ordinal is assigned beyond the
/// committed high-water mark and lives only in the dirty segment) must be
/// resolvable by the same `node_id` that `FIND symbols` returns — without a
/// COMMIT. `find_node` previously resolved ordinals against the committed
/// segment only, so a just-created node failed with "node_id not found".
#[test]
fn find_node_resolves_newly_created_dirty_node() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();
    let file = worktree.join("newnode.cpp");
    std::fs::write(&file, "void AlphaFn() {}\n").expect("write");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table = index_at_path(&CppLanguage, &file);
    let cid = build_segment(&table, &file, seg_dir.parent().unwrap());
    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file.clone(), cid);

    let overlay_path = overlay_dir.join("newnode.bin");
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

    // Add a brand-new function and reindex — ZetaFn lands only in the dirty
    // segment with a fresh ordinal beyond AlphaFn.
    std::fs::write(&file, "void AlphaFn() {}\nvoid ZetaFn() {}\n").expect("rewrite");
    storage
        .reindex_files(std::slice::from_ref(&file))
        .expect("reindex");

    // FIND symbols hands out a node_id for the new node.
    let results = storage
        .find_symbols(&Clauses::default(), &worktree)
        .expect("find_symbols");
    let zeta = results
        .iter()
        .find(|m| m.name == "ZetaFn")
        .expect("ZetaFn must be indexed after reindex");
    let node_id = zeta.node_id.clone().expect("ZetaFn must have a node_id");

    // That exact node_id must resolve via find_node (failed pre-fix).
    let resolved = storage.find_node(&node_id, &worktree);
    assert!(
        resolved.is_ok(),
        "find_node must resolve a newly-created dirty node {node_id}; got {resolved:?}"
    );
    let resolved = resolved
        .unwrap()
        .expect("newly-created node should be found");
    assert_eq!(resolved.name, "ZetaFn");
}

/// BUG-011: `SHOW LINES` emits the dirty segment's ordinal for a line, but
/// `find_node` resolved committed-first. When the `OrdinalRemapper` reassigns a
/// committed ordinal to a different node (ambiguous same-name siblings + an
/// insertion), the emitted id and the resolver disagreed, so `CHANGE NODE`
/// edited the wrong line. `find_node` now resolves dirty-first; the round-trip
/// `find_node(find_node_id_at_line(line)).line == line` must hold.
#[test]
fn find_node_round_trips_after_ordinal_reassignment() {
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();
    let file = worktree.join("rt.cpp");
    // Two IDENTICAL `if (cond) { same(); }` siblings, far apart. Identical bodies
    // mean the remapper cannot tell them apart by fingerprint or content hash.
    let v0 = "void F() {\n    if (cond) { same(); }\n    int p1 = 1;\n    int p2 = 2;\n    int p3 = 3;\n    int p4 = 4;\n    int p5 = 5;\n    int p6 = 6;\n    if (cond) { same(); }\n}\n";
    std::fs::write(&file, v0).expect("write");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    let table = index_at_path(&CppLanguage, &file);
    let cid = build_segment(&table, &file, seg_dir.parent().unwrap());
    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file.clone(), cid);

    let overlay_path = overlay_dir.join("rt.bin");
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

    // Insert a third IDENTICAL `if (cond) { same(); }` at the front. The second
    // committed if's ordinal is reassigned to the (now) middle if on line 3,
    // while its committed line (9) is nearest the LAST if on line 10.
    let v1 = "void F() {\n    if (cond) { same(); }\n    if (cond) { same(); }\n    int p1 = 1;\n    int p2 = 2;\n    int p3 = 3;\n    int p4 = 4;\n    int p5 = 5;\n    int p6 = 6;\n    if (cond) { same(); }\n}\n";
    std::fs::write(&file, v1).expect("rewrite");
    storage
        .reindex_files(std::slice::from_ref(&file))
        .expect("reindex");

    // Round-trip invariant: every line SHOW emits a node_id for must resolve
    // (via find_node) back to that same line.
    let mut mismatches = Vec::new();
    for i in 0..v1.lines().count() {
        let line = i + 1;
        if let Some(id) = storage.find_node_id_at_line("rt.cpp", line) {
            let resolved = storage.find_node(&id, &worktree).expect("find_node ok");
            let got = resolved.as_ref().map(|r| r.line);
            eprintln!("line {line}: id={id} -> resolved line={got:?}");
            if got != Some(line) {
                mismatches.push((line, id.clone(), got));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "find_node round-trip broke for (line, id, got): {mismatches:?}"
    );
}
