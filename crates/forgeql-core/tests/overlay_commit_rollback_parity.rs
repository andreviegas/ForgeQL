//! Overlay/columnar parity: commit and rollback.
//!
//! Rollback GCs orphaned staging segments and restores the checkpoint delta;
//! commit promotes staged segments and builds a fresh overlay that a new
//! session reads from the promoted cache.

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

/// PhaseFT3 gate: after a simulated rollback, `reload_dirty_from_delta` GCs
/// orphaned staging segments (those not in the restored delta) and restores
/// only the state from the checkpoint delta.
#[test]
#[allow(clippy::too_many_lines)]
fn rollback_gcs_orphaned_staging_segments() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, DeltaFile};
    use forgeql_lang_cpp::CppLanguage;
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

    let t1 = index_at_path(&CppLanguage, &file1);
    let t2 = index_at_path(&CppLanguage, &file2);
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
                    SegmentReader::open(&seg_dir.join(
                        forgeql_core::storage::columnar::segment_rel_path(
                            &m.source_path,
                            &m.hex_content_id,
                        ),
                    ))
                    .expect("seg"),
                )
            })
            .collect();
        ColumnarStorage::new_unshared(
            worktree.clone(),
            segs,
            ov,
            Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)])),
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

    let name_a_vec = DeltaFile::read_valid_segment_names(&delta_path);
    assert_eq!(
        name_a_vec.len(),
        1,
        "checkpoint must have exactly 1 staged segment"
    );
    let name_a = name_a_vec[0].clone();

    let staging_dir = worktree.join(".forgeql-staging");
    assert!(
        staging_dir.join(&name_a).exists(),
        "staged segment for file1 must exist"
    );

    // ── Post-checkpoint: reindex file2 → staging hex B, delta updated ──
    std::fs::write(&file2, "void AfterCheckpoint2() {}\n").expect("reindex file2");
    storage
        .reindex_files(std::slice::from_ref(&file2))
        .expect("reindex file2");

    let names_after = DeltaFile::read_valid_segment_names(&delta_path);
    assert_eq!(
        names_after.len(),
        2,
        "after second reindex must have 2 staged segments"
    );
    let name_b = names_after
        .iter()
        .find(|n| *n != &name_a)
        .cloned()
        .expect("name_b");
    assert!(
        staging_dir.join(&name_b).exists(),
        "staged segment for file2 must exist before rollback"
    );

    // ── Simulate git reset --hard: restore delta to checkpoint state ──
    std::fs::write(&delta_path, &checkpoint_delta).expect("restore checkpoint delta");

    // ── Rollback: GC orphaned staging + reload from restored delta ──
    storage
        .reload_dirty_from_delta()
        .expect("reload_dirty_from_delta after rollback");

    // file2's staged segment must be GC'd (no longer in the restored delta).
    assert!(
        !staging_dir.join(&name_b).exists(),
        "staged segment for file2 must be removed after rollback GC"
    );
    // file1's staged segment must remain (still in the restored delta).
    assert!(
        staging_dir.join(&name_a).exists(),
        "staged segment for file1 must survive rollback GC"
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
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, DeltaFile};
    use forgeql_lang_cpp::CppLanguage;
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

    let t1 = index_at_path(&CppLanguage, &file1);
    let t2 = index_at_path(&CppLanguage, &file2);
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
                    SegmentReader::open(&seg_dir.join(
                        forgeql_core::storage::columnar::segment_rel_path(
                            &m.source_path,
                            &m.hex_content_id,
                        ),
                    ))
                    .expect("seg"),
                )
            })
            .collect();
        ColumnarStorage::new_unshared(
            worktree.clone(),
            segs,
            ov,
            Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)])),
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
    let ckpt1_names = DeltaFile::read_valid_segment_names(&delta_path);
    assert_eq!(
        ckpt1_names.len(),
        1,
        "checkpoint1 must have 1 staged segment"
    );

    // ── Checkpoint 2: also reindex file2 ──
    std::fs::write(&file2, "void Phase2File2() {}\n").expect("ckpt2 file2");
    storage
        .reindex_files(std::slice::from_ref(&file2))
        .expect("reindex ckpt2");
    let ckpt2_names = DeltaFile::read_valid_segment_names(&delta_path);
    assert_eq!(
        ckpt2_names.len(),
        2,
        "checkpoint2 must have 2 staged segments"
    );

    // ── Rollback to checkpoint 1 (simulate git reset --hard to ckpt1) ──
    std::fs::write(&delta_path, &ckpt1_delta).expect("restore ckpt1 delta");
    storage
        .reload_dirty_from_delta()
        .expect("reload after rollback to ckpt1");

    // Only ckpt1's staged segment should remain in staging.
    let staging_dir = worktree.join(".forgeql-staging");
    for name in &ckpt2_names {
        if !ckpt1_names.contains(name) {
            assert!(
                !staging_dir.join(name).exists(),
                "ckpt2-only segment {name} must be GC'd after rollback to ckpt1"
            );
        }
    }
    for name in &ckpt1_names {
        assert!(
            staging_dir.join(name).exists(),
            "ckpt1 segment {name} must survive rollback to ckpt1"
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
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
    use forgeql_lang_cpp::CppLanguage;
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

    let table1 = index_at_path(&CppLanguage, &file1);
    let table2 = index_at_path(&CppLanguage, &file2);
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
    let bare_hex1_dir = seg_path(&segments_dir, std::path::Path::new("file1.cpp"), &hex1);
    let bare_hex2_dir = seg_path(&segments_dir, std::path::Path::new("file2.cpp"), &hex2);
    std::fs::create_dir_all(bare_hex1_dir.parent().unwrap()).expect("bare hex1 parent");
    std::fs::create_dir_all(bare_hex2_dir.parent().unwrap()).expect("bare hex2 parent");
    let _ = std::fs::copy(
        seg_path(&wt_seg_dir, std::path::Path::new("file1.cpp"), &hex1),
        &bare_hex1_dir,
    )
    .expect("copy hex1");
    let _ = std::fs::copy(
        seg_path(&wt_seg_dir, std::path::Path::new("file2.cpp"), &hex2),
        &bare_hex2_dir,
    )
    .expect("copy hex2");

    // Build ColumnarBuildContext pointing at bare-repo stores.
    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );

    // Open ColumnarStorage backed by the base overlay.
    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
    let overlay = Overlay::open(&base_overlay_path).expect("open base overlay");
    let seg_root = segments_dir.join(vp());
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_root.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &m.source_path,
                        &m.hex_content_id,
                    ),
                ))
                .expect("open seg"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new_unshared(worktree.clone(), segments, overlay, lang_reg);

    // Modify file1 and reindex into the staging dir.
    std::fs::write(&file1, "void UpdatedFunc1() {}\nvoid NewFunc() {}\n").expect("update file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex file1");

    assert_eq!(storage.dirty().added.len(), 1, "must have 1 staged segment");
    let staged_hex = storage.dirty().added[0].reader.content_id_hex();
    let staging_dir = worktree.join(".forgeql-staging");
    // Includes ENRICH_VER for the same reason a committed segment lives under a
    // versioned directory: a staged segment holds index output, so a generation
    // bump has to invalidate it rather than let COMMIT promote stale rows.
    let staged_name = format!(
        "{}-{staged_hex}-v{}.fqsf",
        forgeql_core::node_id::hex_prefix(&forgeql_core::node_id::sha256_of_path("file1.cpp"), 12),
        forgeql_core::storage::columnar::ENRICH_VER
    );
    assert!(
        staging_dir.join(&staged_name).exists(),
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
    let promoted_dir = seg_path(
        &segments_dir,
        std::path::Path::new("file1.cpp"),
        &staged_hex,
    );
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

/// A COMMIT whose base overlay names a segment the store no longer holds must
/// refuse rather than write a smaller overlay. The merge builder used to skip
/// a missing segment file, so the new overlay came out self-consistent and
/// silently smaller, and every later session on that commit dropped the
/// file's rows from every answer. The dirty overlay carries only the edited
/// files — there is nothing at commit time to rebuild the vanished segment
/// from — so refusal is the only honest outcome, and it must name the file.
/// Restoring the segment makes the same COMMIT succeed: nothing about the
/// refusal is sticky.
#[test]
#[allow(clippy::too_many_lines)]
fn a_commit_whose_base_segment_vanished_is_refused() {
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
    use forgeql_lang_cpp::CppLanguage;
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
    let file2 = worktree.join("file2.cpp");
    std::fs::write(&file1, "void BaseFunc1() {}\n").expect("write file1");
    std::fs::write(&file2, "void BaseFunc2() {}\n").expect("write file2");

    let wt_seg_dir = tmp.path().join("segments");
    std::fs::create_dir_all(wt_seg_dir.join("test")).expect("wt seg dir");

    let table1 = index_at_path(&CppLanguage, &file1);
    let table2 = index_at_path(&CppLanguage, &file2);
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

    let base_overlay_path = overlays_dir.join("test").join("base_commit.bin");
    std::fs::create_dir_all(base_overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", wt_seg_dir.clone(), worktree.clone(), segment_map)
        .build_and_persist(&base_overlay_path)
        .expect("base overlay");

    let bare_hex1 = seg_path(&segments_dir, std::path::Path::new("file1.cpp"), &hex1);
    let bare_hex2 = seg_path(&segments_dir, std::path::Path::new("file2.cpp"), &hex2);
    std::fs::create_dir_all(bare_hex1.parent().unwrap()).expect("bare hex1 parent");
    std::fs::create_dir_all(bare_hex2.parent().unwrap()).expect("bare hex2 parent");
    let _ = std::fs::copy(
        seg_path(&wt_seg_dir, std::path::Path::new("file1.cpp"), &hex1),
        &bare_hex1,
    )
    .expect("copy hex1");
    let _ = std::fs::copy(
        seg_path(&wt_seg_dir, std::path::Path::new("file2.cpp"), &hex2),
        &bare_hex2,
    )
    .expect("copy hex2");

    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );

    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
    let overlay = Overlay::open(&base_overlay_path).expect("open base overlay");
    let seg_root = segments_dir.join(vp());
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_root.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &m.source_path,
                        &m.hex_content_id,
                    ),
                ))
                .expect("open seg"),
            )
        })
        .collect();
    let mut storage = ColumnarStorage::new_unshared(worktree, segments, overlay, lang_reg);

    // Stage a dirty edit to file1 — the COMMIT below has real work to do.
    std::fs::write(&file1, "void UpdatedFunc1() {}\n").expect("update file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex file1");
    assert_eq!(storage.dirty().added.len(), 1, "must have 1 staged segment");

    // The GC-reclaim fault: file2's committed segment vanishes from the bare
    // store while the base overlay still names it.
    std::fs::remove_file(&bare_hex2).expect("delete file2's segment");

    let new_oid = "aabbccddeeff001122334455667788990011223344556677aabbccddeeff0011";
    let err = storage
        .commit_dirty(new_oid, &ctx)
        .expect_err("a COMMIT over a vanished base segment must refuse");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("file2.cpp"),
        "the refusal must name the vanished file: {msg}"
    );
    assert!(
        msg.contains("overlay build incomplete"),
        "the refusal must say what would be lost: {msg}"
    );

    // Nothing was clobbered: the base overlay still opens, and no overlay was
    // written for the refused commit.
    assert!(
        Overlay::open(&base_overlay_path).is_ok(),
        "the base overlay must survive a refused COMMIT"
    );
    let new_overlay_path = ctx.overlay_path_for(new_oid);
    assert!(
        !new_overlay_path.exists(),
        "a refused COMMIT must not leave an overlay behind: {}",
        new_overlay_path.display()
    );

    // Restore the segment: the same COMMIT succeeds.
    let _ = std::fs::copy(
        seg_path(&wt_seg_dir, std::path::Path::new("file2.cpp"), &hex2),
        &bare_hex2,
    )
    .expect("restore file2's segment");
    storage
        .commit_dirty(new_oid, &ctx)
        .expect("the same COMMIT must succeed once the segment is back");
    assert!(
        new_overlay_path.exists(),
        "the retried COMMIT must write the overlay it refused before"
    );
}

/// PhaseFT4 gate: a second session opened against the promoted overlay gets a
/// cache hit (`Overlay::open` succeeds) and returns the committed symbols.
#[test]
#[allow(clippy::too_many_lines)]
fn new_session_hits_promoted_overlay_cache() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder};
    use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
    use forgeql_lang_cpp::CppLanguage;
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

    let table1 = index_at_path(&CppLanguage, &file1);
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

    let bare_hex1_dir = seg_path(&segments_dir, std::path::Path::new("file1.cpp"), &hex1);
    std::fs::create_dir_all(bare_hex1_dir.parent().unwrap()).expect("bare hex1 parent");
    let _ = std::fs::copy(
        seg_path(&wt_seg_dir, std::path::Path::new("file1.cpp"), &hex1),
        &bare_hex1_dir,
    )
    .expect("copy hex1");

    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );
    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));

    // Session A: change file1 and commit.
    let seg_root = segments_dir.join(vp());
    let overlay_a = Overlay::open(&base_overlay_path).expect("open base overlay");
    let segments_a: Vec<Arc<SegmentReader>> = overlay_a
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_root.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &m.source_path,
                        &m.hex_content_id,
                    ),
                ))
                .expect("open seg"),
            )
        })
        .collect();
    let mut storage_a = ColumnarStorage::new_unshared(
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
                SegmentReader::open(&seg_root.join(
                    forgeql_core::storage::columnar::segment_rel_path(
                        &m.source_path,
                        &m.hex_content_id,
                    ),
                ))
                .expect("session B: open seg"),
            )
        })
        .collect();
    let storage_b = ColumnarStorage::new_unshared(
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
