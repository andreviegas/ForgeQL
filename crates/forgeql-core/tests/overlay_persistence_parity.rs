//! Overlay/columnar parity: on-disk persistence.
//!
//! External-edit freshness detection, file purge, and the delta round-trip
//! (write, reindex, and survive a simulated restart) must keep the overlay and
//! the legacy view in agreement.

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

/// BUG-001 regression: a committed segment is content-addressed by git blob
/// sha1, so `is_path_fresh` must report it stale the moment the file on disk
/// diverges from the indexed content (HEAD advanced, file reverted while
/// git-clean, or edited outside ForgeQL) and fresh again after a reindex.
/// This is the invariant that stops `CHANGE NODE` from computing a byte range
/// off a stale line and corrupting the file.
#[test]
#[allow(clippy::too_many_lines)]
fn is_path_fresh_detects_external_edit() {
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::git_sha1_provider::git_blob_sha1;

    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().to_path_buf();
    let file = worktree.join("fresh.cpp");
    std::fs::write(&file, "void Alpha() {}\nvoid Beta() {}\n").expect("write file");

    let seg_dir = tmp.path().join("segments").join(vp());
    let overlay_dir = tmp.path().join("overlays");
    std::fs::create_dir_all(&seg_dir).expect("seg_dir");
    std::fs::create_dir_all(&overlay_dir).expect("overlay_dir");

    // Build a git-sha1 content-addressed committed segment, matching the
    // production shadow-write hash, so the freshness compare is meaningful.
    let table = index_at_path(&CppLanguage, &file);
    let bytes = std::fs::read(&file).expect("read");
    let content_id: Vec<u8> = git_blob_sha1(&bytes).to_vec();
    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });
    {
        let mut builder = SegmentBuilder::new("test", &content_id);
        for row in &table.rows {
            let row_id = builder.emit_row(SymbolRow {
                name: table.name_of(row),
                fql_kind: table.fql_kind_of(row),
                language: table.language_of(row),
                line: u32::try_from(row.line).unwrap_or(u32::MAX),
                byte_start: u32::try_from(row.byte_range.start).unwrap_or(u32::MAX),
                byte_end: u32::try_from(row.byte_range.end).unwrap_or(u32::MAX),
                usages_count: row.usages_count,
            });
            if let Some(ordinal) = row.ordinal {
                builder.set_ordinal(row_id, ordinal);
            }
            for (key, val) in table.resolve_fields(&row.fields) {
                builder.set_field(row_id, &key, val.as_str());
            }
        }
        builder
            .flush(&seg_path(
                seg_dir.parent().unwrap(),
                std::path::Path::new("fresh.cpp"),
                &hex,
            ))
            .expect("segment flush");
    }

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(file.clone(), content_id);

    let overlay_path = overlay_dir.join("freshness.bin");
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

    let rel = std::path::Path::new("fresh.cpp");

    // 1. Clean state — committed hash matches disk.
    assert!(
        storage.is_path_fresh(rel, &worktree),
        "freshly indexed file must be fresh"
    );

    // 2. External edit (bypassing ForgeQL) shifts symbols and changes content.
    std::fs::write(
        &file,
        "// injected\n// injected\nvoid Alpha() {}\nvoid Beta() {}\n",
    )
    .expect("rewrite file");
    assert!(
        !storage.is_path_fresh(rel, &worktree),
        "file edited outside ForgeQL must be detected as stale"
    );

    // 3. Reindex rebuilds the dirty segment from current disk content.
    storage
        .reindex_files(std::slice::from_ref(&file))
        .expect("reindex_files");
    assert!(
        storage.is_path_fresh(rel, &worktree),
        "reindexed file must be fresh again"
    );

    // 4. The dirty segment the reindex built is checked against the disk too:
    //    a second rewrite outside ForgeQL — the shape a gate's auto-format
    //    takes, landing after the session's own edit — must read as stale as
    //    well. It used to be taken as fresh unconditionally.
    std::fs::write(&file, "void Alpha() {}\nvoid Beta() {}\n").expect("rewrite file again");
    assert!(
        !storage.is_path_fresh(rel, &worktree),
        "a file rewritten after its dirty segment was built must be stale"
    );
    // 5. And nothing is answered off the stale rows: a line lookup into the
    //    file fabricates no handle from offsets the file no longer holds. The
    //    stale rows put Beta's start past its current end, which underflowed
    //    here before such a row was skipped.
    let refs = storage.innermost_nodes_for_lines("fresh.cpp", &worktree, 1, 4);
    assert!(
        refs.iter().all(Option::is_none),
        "stale offsets must never fabricate handles: {refs:?}"
    );
    // 6. Fresh again once re-indexed, exactly as the committed case was.
    storage
        .reindex_files(std::slice::from_ref(&file))
        .expect("reindex_files again");
    assert!(storage.is_path_fresh(rel, &worktree));
}

/// `purge_file` on `ColumnarStorage` must remove all symbols for the given
/// file while leaving other files' symbols untouched.
#[test]
fn purge_removes_file_symbols() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_lang_cpp::CppLanguage;
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

    let table1 = index_at_path(&CppLanguage, &file1);
    let table2 = index_at_path(&CppLanguage, &file2);
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
    let mut storage = ColumnarStorage::new_unshared(worktree.clone(), segments, overlay, registry);

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
    let _ = dirty.removed_paths.insert(PathBuf::from("src/gone.cpp"));
    let _ = dirty
        .removed_paths
        .insert(PathBuf::from("src/also_gone.rs"));

    DeltaFile::save(&dirty, &delta_path).expect("save delta");
    assert!(delta_path.exists(), "delta file must exist after save");

    // read_valid_segment_names returns staged names (none here — only removals).
    let names = DeltaFile::read_valid_segment_names(&delta_path);
    assert!(
        names.is_empty(),
        "no staged entries → read_valid_segment_names must be empty"
    );

    // Full roundtrip: load back and compare removed_paths.
    let (loaded, needs_reindex) = DeltaFile::load(&delta_path, &staging_dir).expect("load delta");
    assert_eq!(loaded.added.len(), 0, "no staged entries expected");
    assert!(
        needs_reindex.is_empty(),
        "same-generation delta must queue nothing"
    );
    let mut orig_removed: Vec<_> = dirty.removed_paths.iter().cloned().collect();
    let mut loaded_removed: Vec<_> = loaded.removed_paths.iter().cloned().collect();
    orig_removed.sort_unstable();
    loaded_removed.sort_unstable();
    assert_eq!(
        orig_removed, loaded_removed,
        "removed_paths roundtrip mismatch"
    );
}

/// PhaseFT3 gate: `reindex_files` must write `.forgeql-columnar-delta` with the
/// correct staged metadata matching the dirty overlay state.
#[test]
fn reindex_writes_delta_file() {
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
    std::fs::write(&file1, "void SymbolA() {}\n").expect("write file1");
    std::fs::write(&file2, "void SymbolB() {}\n").expect("write file2");

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
    let mut storage = ColumnarStorage::new_unshared(worktree.clone(), segments, overlay, registry);

    let delta_path = worktree.join(".forgeql-columnar-delta");
    assert!(!delta_path.exists(), "delta must not exist before reindex");

    std::fs::write(&file1, "void SymbolC() {}\n").expect("rewrite file1");
    storage
        .reindex_files(std::slice::from_ref(&file1))
        .expect("reindex_files");

    assert!(delta_path.exists(), "delta must exist after reindex");

    // read_valid_segment_names gives us the staged segment file names.
    let names = DeltaFile::read_valid_segment_names(&delta_path);
    assert_eq!(
        names.len(),
        1,
        "expected 1 staged name; got {}",
        names.len()
    );
    assert!(
        !names[0].is_empty(),
        "staged segment file name must be non-empty"
    );

    // Full load: verify source_path and removed_paths.
    let staging_dir = worktree.join(".forgeql-staging");
    let (loaded_dirty, needs_reindex) =
        DeltaFile::load(&delta_path, &staging_dir).expect("load delta");
    assert!(
        needs_reindex.is_empty(),
        "same-generation delta must queue nothing"
    );
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
        !loaded_dirty.removed_paths.is_empty(),
        "removed_paths must be non-empty after shadowing file1"
    );
}

/// PhaseFT3 gate: after a simulated restart, loading the delta file from disk
/// must restore the dirty overlay so query results match the original instance.
#[test]
#[allow(clippy::too_many_lines)]
fn delta_survives_simulated_restart() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_lang_cpp::CppLanguage;
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

    let table1 = index_at_path(&CppLanguage, &file1);
    let table2 = index_at_path(&CppLanguage, &file2);
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
        ColumnarStorage::new_unshared(
            worktree.clone(),
            segments,
            overlay,
            Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)])),
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
