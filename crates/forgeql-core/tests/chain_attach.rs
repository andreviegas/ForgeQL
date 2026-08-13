//! Chain-manifest attach: a commit with no overlay answers from its master
//! overlay plus manifest-seeded changes, identically to a full rebuild of
//! the same effective state — rows AND totals.
//!
//! The read path under test is the ordinary dirty-overlay union; what these
//! tests pin is the seeding: ownership (single owner per path), refusal on
//! inconsistent manifests, restart round-trip, and the byte-identical-twin
//! rule (shadowing one path never shadows another that shares its bytes).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

mod overlay_harness;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use overlay_harness::*;

use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::chain_manifest::{ChainEntry, ChainManifest};
use forgeql_core::storage::columnar::{
    BuildInput, ColumnarBuildContext, ColumnarStorage, OverlayBuilder,
};

/// The hex spelling of a content ID, matching `build_segment`'s layout.
fn hex_of(cid: &[u8]) -> String {
    cid.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

fn test_ctx(segments_dir: &Path, overlays_dir: &Path) -> ColumnarBuildContext {
    let hash_fn: forgeql_core::storage::HashFn = Arc::new(|b: &[u8]| b.to_vec());
    ColumnarBuildContext::new(
        segments_dir.to_path_buf(),
        overlays_dir.to_path_buf(),
        "test",
        hash_fn,
    )
}

fn registry() -> Arc<LanguageRegistry> {
    Arc::new(LanguageRegistry::new(vec![]))
}

const fn empty_input() -> BuildInput<'static> {
    BuildInput {
        table: None,
        prebuilt_segment_map: None,
    }
}

/// Everything one chained-commit scenario needs, built once:
/// a master overlay over `canonical.cpp` + `canonical.rs`, a changed
/// `canonical.cpp` (one extra function) and a new `extra_chain.cpp` in a
/// separate worktree, their segments in the store, and the manifest naming
/// it all. `canonical.rs` is deleted by the chain.
struct ChainFixture {
    _tmp: tempfile::TempDir,
    ctx: ColumnarBuildContext,
    worktree: PathBuf,
    segments_dir: PathBuf,
    /// (abs path, content id) of the changed + new files.
    changed: Vec<(PathBuf, Vec<u8>)>,
    manifest: ChainManifest,
}

const MASTER_COMMIT: &str = "aa11masteraa11";
const CHILD_COMMIT: &str = "bb22childbb22";

fn chain_fixture() -> ChainFixture {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let ctx = test_ctx(&segments_dir, &overlays_dir);

    // Master: the two canonical fixtures.
    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
    let table_rs = index_fixture(&RustLanguage, "canonical.rs");
    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");
    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let rs_cid = build_segment(&table_rs, &rs_path, &segments_dir);
    let mut master_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = master_map.insert(cpp_path, cpp_cid.clone());
    let _ = master_map.insert(rs_path, rs_cid);
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), master_map)
        .build_and_persist(&ctx.overlay_path_for(MASTER_COMMIT))
        .expect("master overlay build");

    // The chain: canonical.cpp changed (one extra function), extra_chain.cpp
    // new, canonical.rs deleted.
    let changed_cpp = worktree.join("canonical.cpp");
    let original = std::fs::read_to_string(fixture_path("canonical.cpp")).unwrap();
    std::fs::write(
        &changed_cpp,
        format!("{original}\nint chain_only_function(int x) {{ return x + 41; }}\n"),
    )
    .unwrap();
    let new_cpp = worktree.join("extra_chain.cpp");
    std::fs::write(&new_cpp, "int chain_new_file_fn() { return 7; }\n").unwrap();

    let table_cpp2 = index_at_path(&CppLanguage, &changed_cpp);
    let table_new = index_at_path(&CppLanguage, &new_cpp);
    let changed_cid = build_segment(&table_cpp2, &changed_cpp, &segments_dir);
    let new_cid = build_segment(&table_new, &new_cpp, &segments_dir);

    let manifest = ChainManifest {
        version: forgeql_core::storage::columnar::chain_manifest::CHAIN_FORMAT_VERSION,
        enrich_ver: forgeql_core::storage::columnar::ENRICH_VER,
        master_commit: MASTER_COMMIT.to_owned(),
        entries: vec![
            ChainEntry {
                source_path: PathBuf::from("canonical.cpp"),
                hex_content_id: hex_of(&changed_cid),
                replaces_hex: hex_of(&cpp_cid),
            },
            ChainEntry {
                source_path: PathBuf::from("extra_chain.cpp"),
                hex_content_id: hex_of(&new_cid),
                replaces_hex: String::new(),
            },
        ],
        removed_paths: vec![
            PathBuf::from("canonical.cpp"),
            PathBuf::from("canonical.rs"),
        ],
        added_paths: vec![],
    };

    ChainFixture {
        _tmp: tmp,
        ctx,
        worktree,
        segments_dir,
        changed: vec![(changed_cpp, changed_cid), (new_cpp, new_cid)],
        manifest,
    }
}

/// The flat rebuild of the chain's effective state, for parity comparison.
fn flat_storage_of_effective_state(fx: &ChainFixture) -> (ColumnarStorage, tempfile::TempDir) {
    use forgeql_core::storage::columnar::SegmentReader;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let flat_tmp = tempfile::TempDir::new().expect("tempdir");
    let flat_overlay_path = flat_tmp.path().join("flat.bin");
    let mut map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    for (path, cid) in &fx.changed {
        let _ = map.insert(path.clone(), cid.clone());
    }
    OverlayBuilder::new("test", fx.segments_dir.clone(), fx.worktree.clone(), map)
        .build_and_persist(&flat_overlay_path)
        .expect("flat overlay build");
    let overlay = Overlay::open(&flat_overlay_path).expect("open flat overlay");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_path(
                    &fx.segments_dir,
                    &m.source_path,
                    &m.hex_content_id,
                ))
                .expect("open segment"),
            )
        })
        .collect();
    (
        ColumnarStorage::new_unshared(fx.worktree.clone(), segs, overlay, registry()),
        flat_tmp,
    )
}

fn chain_attach(fx: &ChainFixture) -> ColumnarStorage {
    fx.manifest
        .save(&fx.ctx.chain_manifest_path_for(CHILD_COMMIT))
        .expect("manifest save");
    ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        fx.worktree.clone(),
        CHILD_COMMIT,
        registry(),
    )
    .expect("chain attach")
}

#[test]
fn a_chained_commit_answers_like_a_full_rebuild_rows_and_total() {
    let fx = chain_fixture();
    let chained = chain_attach(&fx);
    let (flat, _flat_tmp) = flat_storage_of_effective_state(&fx);

    let clauses = Clauses::default();
    let chained_page = chained
        .find_symbols(&clauses, &fx.worktree)
        .expect("chained find");
    let flat_page = flat
        .find_symbols(&clauses, &fx.worktree)
        .expect("flat find");

    assert_eq!(
        columnar_key_tuples(&chained_page),
        columnar_key_tuples(&flat_page),
        "chained rows differ from a flat rebuild of the same state"
    );
    assert_eq!(
        chained_page.total, flat_page.total,
        "chained total differs from a flat rebuild of the same state"
    );
    // The chain's own content is actually served, and the deleted file is not.
    let names: Vec<&str> = chained_page.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"chain_only_function"),
        "changed-file row missing"
    );
    assert!(names.contains(&"chain_new_file_fn"), "new-file row missing");
    assert!(
        !chained_page
            .iter()
            .any(|r| r.path.as_deref() == Some(Path::new("canonical.rs"))),
        "deleted canonical.rs still serves rows"
    );
}

#[test]
fn a_second_attach_restores_the_chain_from_the_delta_without_reseeding() {
    let fx = chain_fixture();
    let first = chain_attach(&fx);
    let clauses = Clauses::default();
    let first_keys = columnar_key_tuples(
        &first
            .find_symbols(&clauses, &fx.worktree)
            .expect("first find"),
    );
    drop(first);

    // Same worktree: the delta file and staging links now exist, so this
    // open restores rather than reseeds — and must serve the same rows.
    let second = ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        fx.worktree.clone(),
        CHILD_COMMIT,
        registry(),
    )
    .expect("second chain attach");
    let second_keys = columnar_key_tuples(
        &second
            .find_symbols(&clauses, &fx.worktree)
            .expect("second find"),
    );
    assert_eq!(first_keys, second_keys, "restart changed the served rows");
}

#[test]
fn an_entry_shadowing_master_without_recording_the_replacement_is_refused() {
    let mut fx = chain_fixture();
    fx.manifest.entries[0].replaces_hex = String::new();
    fx.manifest
        .save(&fx.ctx.chain_manifest_path_for(CHILD_COMMIT))
        .expect("manifest save");
    // The chain refuses; with nothing to build from, the fallback full
    // build cannot produce an overlay either — the attach errors rather
    // than serving one path from two layers.
    let result = ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        fx.worktree.clone(),
        CHILD_COMMIT,
        registry(),
    );
    assert!(result.is_err(), "inconsistent manifest was served");
}

#[test]
fn a_manifest_naming_a_missing_segment_is_refused_not_skipped() {
    let mut fx = chain_fixture();
    fx.manifest.entries[1].hex_content_id = "00000000deadbeef".to_owned();
    fx.manifest
        .save(&fx.ctx.chain_manifest_path_for(CHILD_COMMIT))
        .expect("manifest save");
    let result = ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        fx.worktree.clone(),
        CHILD_COMMIT,
        registry(),
    );
    assert!(
        result.is_err(),
        "a chain with a missing segment must refuse, not serve a smaller index"
    );
}

#[test]
fn a_manifest_from_another_index_generation_is_not_assembled() {
    let mut fx = chain_fixture();
    fx.manifest.enrich_ver += 1;
    fx.manifest
        .save(&fx.ctx.chain_manifest_path_for(CHILD_COMMIT))
        .expect("manifest save");
    let result = ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        fx.worktree.clone(),
        CHILD_COMMIT,
        registry(),
    );
    assert!(result.is_err(), "cross-generation chain was assembled");
}

#[test]
fn shadowing_one_path_never_shadows_its_byte_identical_twin() {
    // Two master files with identical bytes, so identical content per row;
    // the chain replaces only one of them. The twin's rows must survive.
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let ctx = test_ctx(&segments_dir, &overlays_dir);

    let body = "int twin_shared_fn() { return 1; }\n";
    let a = worktree.join("twin_a.cpp");
    let b = worktree.join("twin_b.cpp");
    std::fs::write(&a, body).unwrap();
    std::fs::write(&b, body).unwrap();
    let table_a = index_at_path(&CppLanguage, &a);
    let table_b = index_at_path(&CppLanguage, &b);
    let a_cid = build_segment(&table_a, &a, &segments_dir);
    let b_cid = build_segment(&table_b, &b, &segments_dir);
    let mut master_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = master_map.insert(a, a_cid.clone());
    let _ = master_map.insert(b, b_cid);
    OverlayBuilder::new("test", segments_dir.clone(), worktree.clone(), master_map)
        .build_and_persist(&ctx.overlay_path_for(MASTER_COMMIT))
        .expect("master overlay build");

    // Replace twin_a with a changed version; twin_b untouched.
    let a2 = worktree.join("changed").join("twin_a.cpp");
    std::fs::create_dir_all(a2.parent().unwrap()).unwrap();
    std::fs::write(&a2, "int twin_changed_fn() { return 2; }\n").unwrap();
    let table_a2 = index_at_path(&CppLanguage, &a2);
    let replacement_cid = build_segment(&table_a2, &a2, &segments_dir);

    let manifest = ChainManifest {
        version: forgeql_core::storage::columnar::chain_manifest::CHAIN_FORMAT_VERSION,
        enrich_ver: forgeql_core::storage::columnar::ENRICH_VER,
        master_commit: MASTER_COMMIT.to_owned(),
        entries: vec![ChainEntry {
            source_path: PathBuf::from("twin_a.cpp"),
            hex_content_id: hex_of(&replacement_cid),
            replaces_hex: hex_of(&a_cid),
        }],
        removed_paths: vec![PathBuf::from("twin_a.cpp")],
        added_paths: vec![],
    };
    manifest
        .save(&ctx.chain_manifest_path_for(CHILD_COMMIT))
        .expect("manifest save");

    let storage = ColumnarStorage::warm_or_open(
        &ctx,
        empty_input(),
        worktree.clone(),
        CHILD_COMMIT,
        registry(),
    )
    .expect("chain attach");
    let page = storage
        .find_symbols(&Clauses::default(), &worktree)
        .expect("find");
    let by_file: Vec<(&str, &str)> = page
        .iter()
        .filter_map(|r| Some((r.path.as_deref().and_then(Path::to_str)?, r.name.as_str())))
        .collect();
    assert!(
        by_file.contains(&("twin_b.cpp", "twin_shared_fn")),
        "byte-identical twin was shadowed along with the changed file: {by_file:?}"
    );
    assert!(
        by_file.contains(&("twin_a.cpp", "twin_changed_fn")),
        "changed file does not serve its new rows: {by_file:?}"
    );
    assert!(
        !by_file.contains(&("twin_a.cpp", "twin_shared_fn")),
        "replaced file still serves its old rows: {by_file:?}"
    );
}

#[test]
fn seeded_sessions_stream_the_ascending_shapes_with_an_honest_total() {
    use forgeql_core::ir::{OrderBy, SortDirection};

    let fx = chain_fixture();
    let chained = chain_attach(&fx);
    let (flat, _flat_tmp) = flat_storage_of_effective_state(&fx);

    // The bare LIMIT shape rides the merged stream on a seeded session; the
    // flat rebuild of the same state is the ground truth for rows and total.
    let bare = Clauses {
        limit: Some(3),
        ..Clauses::default()
    };
    let chained_page = chained
        .find_symbols(&bare, &fx.worktree)
        .expect("chained bare");
    let flat_page = flat.find_symbols(&bare, &fx.worktree).expect("flat bare");
    assert_eq!(
        columnar_key_tuples(&chained_page),
        columnar_key_tuples(&flat_page),
        "bare LIMIT page differs between merged stream and flat rebuild"
    );
    assert_eq!(
        chained_page.total, flat_page.total,
        "bare LIMIT total differs between merged stream and flat rebuild"
    );

    // ORDER BY name ASC rides the same merged stream.
    let asc = Clauses {
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(4),
        ..Clauses::default()
    };
    let chained_page = chained
        .find_symbols(&asc, &fx.worktree)
        .expect("chained asc");
    let flat_page = flat.find_symbols(&asc, &fx.worktree).expect("flat asc");
    assert_eq!(
        columnar_key_tuples(&chained_page),
        columnar_key_tuples(&flat_page),
        "ORDER BY name page differs between merged stream and flat rebuild"
    );
    assert_eq!(
        chained_page.total, flat_page.total,
        "ORDER BY name total differs between merged stream and flat rebuild"
    );
}

#[test]
fn the_merged_stream_matches_the_seeded_pipeline() {
    use forgeql_core::ir::{CompareOp, OrderBy, Predicate, PredicateValue, SortDirection};

    // Both routes run on the SAME seeded session: the streamed shape, and a
    // pipeline twin whose WHERE matches every row (lines are 1-based) but
    // declines the stream. If the merged stream ever drifted from what the
    // seeded pipeline serves, this is the test that names it — comparing two
    // streams against each other could not.
    let fx = chain_fixture();
    let chained = chain_attach(&fx);

    let streamed = Clauses {
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(4),
        ..Clauses::default()
    };

    let mut pipeline = streamed.clone();
    pipeline.where_predicates = vec![Predicate {
        field: "line".to_owned(),
        op: CompareOp::Gte,
        value: PredicateValue::Number(1),
    }];

    let streamed_page = chained
        .find_symbols(&streamed, &fx.worktree)
        .expect("streamed");
    let pipeline_page = chained
        .find_symbols(&pipeline, &fx.worktree)
        .expect("pipeline");
    assert_eq!(
        columnar_key_tuples(&streamed_page),
        columnar_key_tuples(&pipeline_page),
        "merged stream page differs from the seeded pipeline"
    );
    assert_eq!(
        streamed_page.total, pipeline_page.total,
        "merged stream total differs from the seeded pipeline"
    );
}

/// Compaction probe — run by `a_chain_past_the_threshold_compacts_into_a_full_overlay`
/// in a child process with `FORGEQL_CHAIN_COMPACT_PATHS=1`, so the threshold
/// trips on the fixture chain without touching this process's environment.
#[test]
#[ignore = "driven by a_chain_past_the_threshold_compacts_into_a_full_overlay with a scoped env var"]
fn chain_compaction_probe() {
    let fx = chain_fixture();
    let manifest_path = fx.ctx.chain_manifest_path_for(CHILD_COMMIT);
    fx.manifest.save(&manifest_path).expect("manifest save");
    let storage = ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        fx.worktree.clone(),
        CHILD_COMMIT,
        registry(),
    )
    .expect("compacted attach");
    // The chain compacted: a full overlay exists and the manifest is gone.
    assert!(
        fx.ctx.overlay_path_for(CHILD_COMMIT).exists(),
        "no overlay written at the compaction threshold"
    );
    assert!(!manifest_path.exists(), "superseded manifest survived");
    // And the compacted overlay serves the effective state, rows and total.
    let (flat, _flat_tmp) = flat_storage_of_effective_state(&fx);
    let clauses = Clauses::default();
    let compacted = storage.find_symbols(&clauses, &fx.worktree).expect("find");
    let flat_page = flat.find_symbols(&clauses, &fx.worktree).expect("flat");
    assert_eq!(
        columnar_key_tuples(&compacted),
        columnar_key_tuples(&flat_page),
        "compacted overlay differs from a flat rebuild of the same state"
    );
    assert_eq!(compacted.total, flat_page.total, "compacted total differs");
}

#[test]
fn a_chain_past_the_threshold_compacts_into_a_full_overlay() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(&exe);
    let _ = cmd.args(["--exact", "chain_compaction_probe", "--ignored"]);
    let _ = cmd.env("FORGEQL_CHAIN_COMPACT_PATHS", "1");
    let out = cmd.output().expect("spawn probe");
    assert!(
        out.status.success(),
        "compaction probe failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
