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

// ── Upstream commits: the chain the attach derives for itself ───────────────
//
// A commit that arrived by REFRESH SOURCE has no manifest. The attach derives
// one from the nearest ancestor overlay and its own segment map, and must
// then serve exactly what a full build of the commit serves — row for row,
// across every verb, with a deletion and a rename in the change set: the
// two shapes a count cannot tell apart from a row that merely moved.

/// A git repository with two commits on `main`, laid out as a REFRESH would
/// leave them: `a` has an overlay, its child `b` has nothing. Between them
/// `canonical.cpp` is edited, `canonical.rs` is deleted, `moved_from.cpp`
/// is renamed to `moved_to.cpp`, `fresh.cpp` is added; and among the files
/// no plugin claims, `notes.txt` changes size, `gone.txt` is deleted and
/// `new.txt` is added.
struct UpstreamFixture {
    tmp: tempfile::TempDir,
    ctx: ColumnarBuildContext,
    worktree: PathBuf,
    segments_dir: PathBuf,
    repo: git2::Repository,
    commit_a: String,
    commit_b: String,
    /// The segment map the parse of `b` produces: absolute path → content id.
    map_b: HashMap<PathBuf, Vec<u8>>,
}

/// A content id derived from the file's bytes, so the same path holding
/// different bytes at two commits keys two different segments — the property
/// the derivation reads the change set off.
fn content_id_of(bytes: &[u8]) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish().to_le_bytes().to_vec()
}

/// Index every claimed file under `worktree` (by extension: `.cpp` and
/// `.rs`), write its segment, and return the abs-path → content-id map the
/// inline parse would have produced.
fn index_worktree(worktree: &Path, segments_dir: &Path) -> HashMap<PathBuf, Vec<u8>> {
    let mut map = HashMap::new();
    for entry in std::fs::read_dir(worktree).expect("read worktree") {
        let path = entry.expect("dir entry").path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let table = match ext {
            "cpp" => index_at_path(&CppLanguage, &path),
            "rs" => index_at_path(&RustLanguage, &path),
            _ => continue,
        };
        let cid = content_id_of(&std::fs::read(&path).expect("read source"));
        let rel = path.strip_prefix(worktree).expect("under worktree");
        build_segment_with_id(&table, rel, &cid, segments_dir);
        let _ = map.insert(path, cid);
    }
    map
}

fn git_commit_all(repo: &git2::Repository, message: &str) -> String {
    let mut index = repo.index().expect("index");
    index
        .add_all(["."], git2::IndexAddOption::DEFAULT, None)
        .expect("add");
    index.update_all(["."], None).expect("update");
    index.write().expect("write index");
    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("tree");
    let parent = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let secs = parent.as_ref().map_or(0, |p| p.time().seconds() + 60);
    let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(secs, 0))
        .expect("signature");
    let parents: Vec<&git2::Commit> = parent.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)
        .expect("commit")
        .to_string()
}

fn upstream_fixture() -> UpstreamFixture {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let ctx = test_ctx(&segments_dir, &overlays_dir);
    let repo = git2::Repository::init(&worktree).expect("git init");
    {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
    }
    // A real session worktree keeps ForgeQL's runtime artefacts out of every
    // walk through the managed block in `.git/info/exclude`, and its `.git`
    // is a gitfile the walk treats as one more artefact. This fixture repo
    // has neither, so say the same thing in the ignore file the walk reads.
    std::fs::write(
        worktree.join(".forgeql-ignore"),
        ".git/\n.forgeql-session\n.forgeql-staging/\n.forgeql-showmore*\n.forgeql-patches/\n",
    )
    .unwrap();

    // Commit a.
    let original_cpp = std::fs::read_to_string(fixture_path("canonical.cpp")).unwrap();
    std::fs::write(worktree.join("canonical.cpp"), &original_cpp).unwrap();
    let _ = std::fs::copy(fixture_path("canonical.rs"), worktree.join("canonical.rs")).unwrap();
    std::fs::write(
        worktree.join("moved_from.cpp"),
        "int mover(int y) { return factorial(y) + bar(y); }\n",
    )
    .unwrap();
    std::fs::write(worktree.join("notes.txt"), "notes v1\n").unwrap();
    std::fs::write(worktree.join("gone.txt"), "gone at b\n").unwrap();
    let commit_a = git_commit_all(&repo, "a");
    let map_a = index_worktree(&worktree, &segments_dir);
    OverlayBuilder::new("test", segments_dir.clone(), worktree.clone(), map_a)
        .build_and_persist(&ctx.overlay_path_for(&commit_a))
        .expect("master overlay build");

    // Commit b: edit, delete, rename, add — indexed and non-indexed alike.
    std::fs::write(
        worktree.join("canonical.cpp"),
        format!(
            "{original_cpp}\nint upstream_only_function(int x) {{ return factorial(x) + 41; }}\n"
        ),
    )
    .unwrap();
    std::fs::remove_file(worktree.join("canonical.rs")).unwrap();
    std::fs::rename(
        worktree.join("moved_from.cpp"),
        worktree.join("moved_to.cpp"),
    )
    .unwrap();
    std::fs::write(
        worktree.join("fresh.cpp"),
        "int fresh_fn() { return bar(3); }\n",
    )
    .unwrap();
    std::fs::write(worktree.join("notes.txt"), "notes version two\n").unwrap();
    std::fs::remove_file(worktree.join("gone.txt")).unwrap();
    std::fs::write(worktree.join("new.txt"), "new at b\n").unwrap();
    let commit_b = git_commit_all(&repo, "b");
    let map_b = index_worktree(&worktree, &segments_dir);

    UpstreamFixture {
        tmp,
        ctx,
        worktree,
        segments_dir,
        repo,
        commit_a,
        commit_b,
        map_b,
    }
}

/// A full build of `map` — the ground truth a chain must match.
fn full_build_storage(
    fx: &UpstreamFixture,
    map: &HashMap<PathBuf, Vec<u8>>,
) -> (ColumnarStorage, tempfile::TempDir) {
    use forgeql_core::storage::columnar::SegmentReader;
    use forgeql_core::storage::columnar::overlay::Overlay;

    let flat_tmp = tempfile::TempDir::new().expect("tempdir");
    let flat_overlay_path = flat_tmp.path().join("flat.bin");
    OverlayBuilder::new(
        "test",
        fx.segments_dir.clone(),
        fx.worktree.clone(),
        map.clone(),
    )
    .build_and_persist(&flat_overlay_path)
    .expect("full overlay build");
    let overlay = Overlay::open(&flat_overlay_path).expect("open full overlay");
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

fn attach_upstream(
    fx: &UpstreamFixture,
    map: Option<HashMap<PathBuf, Vec<u8>>>,
) -> ColumnarStorage {
    ColumnarStorage::warm_or_open(
        &fx.ctx,
        BuildInput {
            table: None,
            prebuilt_segment_map: map,
        },
        fx.worktree.clone(),
        &fx.commit_b,
        registry(),
    )
    .expect("upstream attach")
}

/// Every column of a symbol row, `usages` included: on a seeded session it is
/// the master aggregate corrected for the seeded change — the commit's own
/// count — see `usages_on_a_derived_chain_is_the_commits_own_count`.
type RowProjection = (
    String,
    usize,
    String,
    String,
    String,
    Option<usize>,
    Vec<(String, String)>,
);

fn projected(rows: &[SymbolMatch]) -> Vec<RowProjection> {
    let mut v: Vec<RowProjection> = rows
        .iter()
        .map(|r| {
            let mut fields: Vec<(String, String)> = r
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            fields.sort();
            (
                r.path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                r.line.unwrap_or(0),
                r.name.clone(),
                r.fql_kind.clone().unwrap_or_default(),
                r.language.clone().unwrap_or_default(),
                r.usages_count,
                fields,
            )
        })
        .collect();
    v.sort();
    v
}

fn eq_predicate(field: &str, value: &str) -> forgeql_core::ir::Predicate {
    forgeql_core::ir::Predicate {
        field: field.to_owned(),
        op: forgeql_core::ir::CompareOp::Eq,
        value: forgeql_core::ir::PredicateValue::String(value.to_owned()),
    }
}

#[test]
fn an_upstream_commit_attaches_through_a_derived_chain_not_a_full_build() {
    use forgeql_core::storage::columnar::chain_manifest::ChainManifest;

    let fx = upstream_fixture();
    let _storage = attach_upstream(&fx, Some(fx.map_b.clone()));

    // Served as a chain: no overlay was built for b, and the manifest the
    // attach derived is on disk beside where one would be, naming a as its
    // master and the change set exactly.
    assert!(
        !fx.ctx.overlay_path_for(&fx.commit_b).exists(),
        "an upstream attach built a full overlay instead of chaining"
    );
    let manifest_path = fx.ctx.chain_manifest_path_for(&fx.commit_b);
    let manifest = ChainManifest::load(&manifest_path).expect("derived manifest on disk");
    assert_eq!(manifest.master_commit, fx.commit_a);
    let mut entries: Vec<(String, bool)> = manifest
        .entries
        .iter()
        .map(|e| {
            (
                e.source_path.display().to_string(),
                e.replaces_hex.is_empty(),
            )
        })
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        vec![
            ("canonical.cpp".to_owned(), false),
            ("fresh.cpp".to_owned(), true),
            ("moved_to.cpp".to_owned(), true),
        ],
        "entries: (path, is a new path)"
    );
    let removed: Vec<String> = manifest
        .removed_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(
        removed,
        vec![
            "canonical.cpp",
            "canonical.rs",
            "gone.txt",
            "moved_from.cpp"
        ]
    );
    let added: Vec<String> = manifest
        .added_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(added, vec!["new.txt", "notes.txt"]);

    // A later attach — one that brings no segment map at all — is served
    // from the written manifest and still builds nothing.
    let _again = attach_upstream(&fx, None);
    assert!(!fx.ctx.overlay_path_for(&fx.commit_b).exists());
}

#[test]
fn a_derived_chain_answers_every_verb_like_a_full_build_of_the_commit() {
    let fx = upstream_fixture();
    let chained = attach_upstream(&fx, Some(fx.map_b.clone()));
    let (full, _full_tmp) = full_build_storage(&fx, &fx.map_b);
    let root = &fx.worktree;

    // FIND symbols — every row, every column but usages.
    let all = Clauses::default();
    let c = chained.find_symbols(&all, root).expect("chained find");
    let f = full.find_symbols(&all, root).expect("full find");
    assert_eq!(projected(&c), projected(&f), "FIND symbols rows differ");
    assert_eq!(c.total, f.total, "FIND symbols total differs");
    assert!(!c.is_empty(), "the comparison ran over no rows");
    // The change set is really in the answer: the edit, the new file, the
    // rename's new path — and neither the deleted file nor the old path.
    let paths: Vec<String> = c
        .iter()
        .filter_map(|r| r.path.as_ref().map(|p| p.display().to_string()))
        .collect();
    let names: Vec<&str> = c.iter().map(|r| r.name.as_str()).collect();
    assert!(
        names.contains(&"upstream_only_function"),
        "edited file not served"
    );
    assert!(names.contains(&"fresh_fn"), "added file not served");
    assert!(
        paths.iter().any(|p| p == "moved_to.cpp"),
        "renamed file not served"
    );
    assert!(
        !paths.iter().any(|p| p == "moved_from.cpp"),
        "old path of a rename served"
    );
    assert!(
        !paths.iter().any(|p| p == "canonical.rs"),
        "deleted file served"
    );

    // An enrichment predicate and a kind predicate.
    for clauses in [
        Clauses {
            where_predicates: vec![eq_predicate("naming", "snake_case")],
            ..Clauses::default()
        },
        Clauses {
            where_predicates: vec![eq_predicate("fql_kind", "function")],
            ..Clauses::default()
        },
    ] {
        let c = chained.find_symbols(&clauses, root).expect("chained find");
        let f = full.find_symbols(&clauses, root).expect("full find");
        assert!(!f.is_empty(), "predicate matched nothing: {clauses:?}");
        assert_eq!(
            projected(&c),
            projected(&f),
            "rows differ under {clauses:?}"
        );
        assert_eq!(c.total, f.total, "total differs under {clauses:?}");
    }

    // FIND usages — a name whose sites moved (rename), grew (edit) and
    // appeared (new file).
    for name in ["factorial", "bar"] {
        let (c, c_note) = chained
            .find_usages(name, &all, root)
            .expect("chained usages");
        let (f, f_note) = full.find_usages(name, &all, root).expect("full usages");
        assert!(!f.is_empty(), "no usage sites for {name}");
        assert_eq!(
            projected(&c),
            projected(&f),
            "FIND usages OF {name} differs"
        );
        assert_eq!(c_note, f_note);
    }

    // FIND files — indexed and non-indexed, path and size.
    let files = |s: &ColumnarStorage| {
        let mut v: Vec<(String, u64)> = s
            .indexed_files()
            .expect("file list")
            .into_iter()
            .map(|e| (e.path.display().to_string(), e.size))
            .collect();
        v.sort();
        v
    };
    let c_files = files(&chained);
    assert_eq!(c_files, files(&full), "FIND files differs");
    assert!(c_files.iter().any(|(p, _)| p == "new.txt"));
    let notes_len = u64::try_from("notes version two\n".len()).unwrap();
    assert!(
        c_files
            .iter()
            .any(|(p, s)| p == "notes.txt" && *s == notes_len)
    );
    assert!(!c_files.iter().any(|(p, _)| p == "gone.txt"));
    assert!(!c_files.iter().any(|(p, _)| p == "moved_from.cpp"));

    // SHOW outline — the edited file and the renamed one.
    let workspace = forgeql_core::workspace::Workspace::new(root).expect("workspace");
    for file in ["canonical.cpp", "moved_to.cpp"] {
        let c = chained
            .show_outline_for_file(&workspace, file, false)
            .expect("chained outline");
        let f = full
            .show_outline_for_file(&workspace, file, false)
            .expect("full outline");
        assert_eq!(c, f, "SHOW outline OF {file} differs");
    }
}

#[test]
fn a_sibling_branch_overlay_is_not_a_base() {
    let fx = upstream_fixture();
    // Move the only overlay to a commit that is not an ancestor of b: a
    // sibling off a. Same bytes as a's overlay — the point is the graph.
    let a_commit = fx
        .repo
        .find_commit(git2::Oid::from_str(&fx.commit_a).unwrap())
        .unwrap();
    let side_oid = {
        let sig = git2::Signature::new("test", "test@test.com", &git2::Time::new(1, 0)).unwrap();
        fx.repo
            .commit(
                None,
                &sig,
                &sig,
                "side",
                &a_commit.tree().unwrap(),
                &[&a_commit],
            )
            .unwrap()
    };
    let _ = fx
        .repo
        .reference("refs/heads/side", side_oid, true, "side")
        .unwrap();
    let side_overlay = fx.ctx.overlay_path_for(&side_oid.to_string());
    std::fs::create_dir_all(side_overlay.parent().unwrap()).unwrap();
    std::fs::rename(fx.ctx.overlay_path_for(&fx.commit_a), &side_overlay).unwrap();

    let _storage = attach_upstream(&fx, Some(fx.map_b.clone()));
    assert!(
        fx.ctx.overlay_path_for(&fx.commit_b).exists(),
        "no ancestor had an overlay, so b needed a full build"
    );
    assert!(!fx.ctx.chain_manifest_path_for(&fx.commit_b).exists());
}

#[test]
fn a_change_set_past_the_threshold_takes_the_full_build() {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(&exe);
    let _ = cmd.args(["--exact", "upstream_threshold_probe", "--ignored"]);
    let _ = cmd.env("FORGEQL_CHAIN_COMPACT_PATHS", "1");
    let out = cmd.output().expect("spawn probe");
    assert!(
        out.status.success(),
        "threshold probe failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Threshold probe — run by `a_change_set_past_the_threshold_takes_the_full_build`
/// in a child process with `FORGEQL_CHAIN_COMPACT_PATHS=1`, where the fixture's
/// change set is already too large to seed.
#[test]
#[ignore = "driven by a_change_set_past_the_threshold_takes_the_full_build with a scoped env var"]
fn upstream_threshold_probe() {
    let fx = upstream_fixture();
    let storage = attach_upstream(&fx, Some(fx.map_b.clone()));
    assert!(
        fx.ctx.overlay_path_for(&fx.commit_b).exists(),
        "past the threshold the attach must build the full overlay"
    );
    assert!(
        !fx.ctx.chain_manifest_path_for(&fx.commit_b).exists(),
        "a manifest was written for a change set the chain refused"
    );
    let (full, _full_tmp) = full_build_storage(&fx, &fx.map_b);
    let clauses = Clauses::default();
    let c = storage.find_symbols(&clauses, &fx.worktree).expect("find");
    let f = full.find_symbols(&clauses, &fx.worktree).expect("find");
    assert_eq!(projected(&c), projected(&f));
}

#[test]
fn a_path_indexed_in_master_but_not_in_the_target_is_refused_not_guessed() {
    // The target's segment map omits canonical.cpp while the file is still
    // on disk: to the derivation it is an indexed file in the master and a
    // non-indexed one in the target, a shape the dirty overlay cannot hold.
    // The attach must refuse the chain and build the full overlay instead.
    let fx = upstream_fixture();
    let mut partial = fx.map_b.clone();
    let _ = partial.remove(&fx.worktree.join("canonical.cpp"));
    let _storage = attach_upstream(&fx, Some(partial));
    assert!(fx.ctx.overlay_path_for(&fx.commit_b).exists());
    assert!(!fx.ctx.chain_manifest_path_for(&fx.commit_b).exists());
}

#[test]
fn usages_on_a_derived_chain_is_the_commits_own_count() {
    // `usages` is stamped from the master overlay's aggregate, corrected on
    // a seeded session for the sites the seeded change shadowed and added.
    // `factorial` gains a site in the edited canonical.cpp and keeps one in
    // the renamed file, so its count at b differs from its count at a — the
    // pin that shows the correction did something, beside the equality that
    // shows it did the right thing.
    let fx = upstream_fixture();
    let chained = attach_upstream(&fx, Some(fx.map_b.clone()));
    let (full, _full_tmp) = full_build_storage(&fx, &fx.map_b);
    // A session on a is a session of its own, so it gets a worktree of its
    // own: the chained session left its delta file in `fx.worktree`, and an
    // attach there would restore that delta as a's dirty state.
    let master_wt = fx.tmp.path().join("master-worktree");
    std::fs::create_dir_all(&master_wt).unwrap();
    let master =
        ColumnarStorage::warm_or_open(&fx.ctx, empty_input(), master_wt, &fx.commit_a, registry())
            .expect("attach to a");
    let clauses = Clauses {
        where_predicates: vec![eq_predicate("name", "factorial")],
        ..Clauses::default()
    };
    let usages_of = |s: &ColumnarStorage| {
        s.find_symbols(&clauses, &fx.worktree)
            .expect("find")
            .iter()
            .find(|r| r.fql_kind.as_deref() == Some("function"))
            .and_then(|r| r.usages_count)
    };
    let (c, f, a) = (usages_of(&chained), usages_of(&full), usages_of(&master));
    assert!(
        f.is_some_and(|n| n > 0),
        "the fixture carries no usage sites"
    );
    assert_eq!(c, f, "usages on the chain differs from the full build");
    assert_ne!(
        c, a,
        "usages did not change between a and b — the pin is vacuous"
    );
}

#[test]
fn a_delta_left_by_an_older_chain_is_not_the_new_commits_state() {
    // The REFRESH shape: a worktree that chained at b keeps its delta file
    // (an ignored runtime file) when git fast-forwards it to c. c has no
    // overlay and no manifest, so the attach derives one — from a, since b
    // only ever had a manifest — and finds b's seed already restored. That
    // seed is a's-plus-b's-changes and must not stand in for c: an attach
    // that kept it would serve b's rows and drop everything c changed.
    let fx = upstream_fixture();
    let _chained_at_b = attach_upstream(&fx, Some(fx.map_b.clone()));
    assert!(
        fx.worktree.join(".forgeql-columnar-delta").exists(),
        "the chained session left no delta to linger — the scenario is vacuous"
    );

    // Move the worktree on to c: one more function in fresh.cpp.
    std::fs::write(
        fx.worktree.join("fresh.cpp"),
        "int fresh_fn() { return bar(3); }\nint fresh_second() { return factorial(2); }\n",
    )
    .unwrap();
    let commit_c = git_commit_all(&fx.repo, "c");
    let map_c = index_worktree(&fx.worktree, &fx.segments_dir);
    let mut chained_at_c = ColumnarStorage::warm_or_open(
        &fx.ctx,
        BuildInput {
            table: None,
            prebuilt_segment_map: Some(map_c.clone()),
        },
        fx.worktree.clone(),
        &commit_c,
        registry(),
    )
    .expect("attach at c");
    assert!(
        !fx.ctx.overlay_path_for(&commit_c).exists(),
        "c was full-built rather than chained"
    );
    let (full, _full_tmp) = full_build_storage(&fx, &map_c);
    let all = Clauses::default();
    let c = chained_at_c
        .find_symbols(&all, &fx.worktree)
        .expect("chained find");
    let f = full.find_symbols(&all, &fx.worktree).expect("full find");
    assert!(
        c.iter().any(|r| r.name == "fresh_second"),
        "c's change was not served: b's lingering delta stood in for c's chain"
    );
    assert_eq!(
        projected(&c),
        projected(&f),
        "rows at c differ from a full build of c"
    );
    assert_eq!(c.total, f.total);
    // Every path b's delta named is queued for re-index, so a live worktree
    // that reaches this attach through a reconnect re-indexes them from disk
    // — b's seeded files, its shadowed paths, its non-indexed additions.
    let queued = chained_at_c.take_pending_reindex_paths();
    for p in [
        "canonical.cpp",
        "fresh.cpp",
        "moved_to.cpp",
        "canonical.rs",
        "notes.txt",
    ] {
        assert!(
            queued.contains(&PathBuf::from(p)),
            "{p} from the dropped delta was not queued for re-index: {queued:?}"
        );
    }
}

#[test]
fn a_delta_without_its_staging_is_reseeded_and_its_paths_stay_queued() {
    // A checkpoint tree carries the committing session's delta file and none
    // of its staging, so a fresh checkout of it restores a delta whose every
    // segment is missing: nothing usable, everything queued for a re-index
    // that only a reconnect runs — and a fresh worktree never reconnects.
    // Such a delta must not stand in for the chain state: the attach seeds.
    let fx = chain_fixture();
    let _seeded = chain_attach(&fx);
    let delta = fx.worktree.join(".forgeql-columnar-delta");
    assert!(
        delta.exists(),
        "the seeded session wrote no delta — vacuous"
    );

    let wt2 = fx.worktree.parent().unwrap().join("fresh-checkout");
    std::fs::create_dir_all(&wt2).unwrap();
    for name in ["canonical.cpp", "extra_chain.cpp"] {
        let _ = std::fs::copy(fx.worktree.join(name), wt2.join(name)).unwrap();
    }
    let _ = std::fs::copy(&delta, wt2.join(".forgeql-columnar-delta")).unwrap();

    let mut attached = ColumnarStorage::warm_or_open(
        &fx.ctx,
        empty_input(),
        wt2.clone(),
        CHILD_COMMIT,
        registry(),
    )
    .expect("attach in the fresh checkout");
    let (flat, _flat_tmp) = flat_storage_of_effective_state(&fx);
    let clauses = Clauses::default();
    let a = attached.find_symbols(&clauses, &wt2).expect("find");
    let f = flat
        .find_symbols(&clauses, &fx.worktree)
        .expect("flat find");
    assert!(
        a.iter().any(|r| r.name == "chain_only_function"),
        "the chain was not seeded: the empty restore was taken for chain state"
    );
    assert_eq!(columnar_key_tuples(&a), columnar_key_tuples(&f));
    assert_eq!(a.total, f.total);
    // The dropped delta's paths stay queued: a live worktree's reconnect
    // re-indexes them from disk, which is how a file created in-session and
    // never committed — untracked, so outside the reconnect's diff against
    // HEAD — gets its rows back. A fresh checkout never drains the queue and
    // is served by the seed; the queue costs it nothing.
    let mut queued = attached.take_pending_reindex_paths();
    queued.sort();
    assert_eq!(
        queued,
        vec![
            PathBuf::from("canonical.cpp"),
            PathBuf::from("canonical.rs"),
            PathBuf::from("extra_chain.cpp")
        ],
        "the dropped delta's paths were forgotten instead of queued"
    );
}
