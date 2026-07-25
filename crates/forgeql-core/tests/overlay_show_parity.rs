//! Overlay/columnar parity: SHOW surface.
//!
//! `show_outline`, `show_body`, `show_signature`, `show_members`,
//! `show_context`, and `show_callees` on the columnar backend must match the
//! legacy engine — including the bare-repo path where a file's bytes are read
//! from the git blob rather than from disk.

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

/// Verify that `ColumnarStorage::show_outline_for_file` returns the same
/// (name, fql_kind, line) set as the legacy `show_outline`.
#[test]
fn columnar_show_outline_matches_legacy() {
    use forgeql_core::ast::show::show_outline;
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::ColumnarStorage;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::workspace::Workspace;

    let table = index_fixture(&CppLanguage, "canonical.cpp");
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
                SegmentReader::open(&seg_path(
                    &segments_dir,
                    &meta.source_path,
                    &meta.hex_content_id,
                ))
                .expect("SegmentReader::open"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new(
        fixtures_dir(),
        segments,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );

    // -- columnar result
    let columnar_json = storage
        .show_outline_for_file(&workspace, "canonical.cpp", true)
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

/// Verify `SHOW body` on the columnar backend emits the same `start_line` as legacy.
#[test]
fn columnar_show_body_matches_legacy() {
    use forgeql_core::ast::lang::{LanguageRegistry, LanguageSupport};
    use forgeql_core::ast::show::{ShowRequest, show_body};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Columnar path
    let col_loc = storage
        .resolve_body_symbol("process", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("process not found (columnar)");
    let col_req = ShowRequest {
        cached: &cached,
        path: &col_loc.path,
        byte_range_start: col_loc.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "process",
        lang_registry: &registry,
        ordinal: None,
    };
    let col_json = show_body(&col_req, Some(0), &col_loc.enrichment).expect("columnar show_body");

    // Legacy path
    let leg_row = table
        .find_def("process")
        .expect("process not found (legacy)");
    let leg_enrichment = table.resolve_fields(&leg_row.fields);
    let leg_req = ShowRequest {
        cached: &cached,
        path: &cpp_path,
        byte_range_start: leg_row.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "process",
        lang_registry: &registry,
        ordinal: None,
    };
    let leg_json = show_body(&leg_req, Some(0), &leg_enrichment).expect("legacy show_body");

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
    use forgeql_core::ast::show::{ShowRequest, show_signature};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Columnar
    let col_loc = storage
        .resolve_symbol("process", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("process not found (columnar)");
    let col_req = ShowRequest {
        cached: &cached,
        path: &col_loc.path,
        byte_range_start: col_loc.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "process",
        lang_registry: &registry,
        ordinal: None,
    };
    let col_json = show_signature(&col_req, &col_loc.node_kind).expect("columnar show_signature");

    // Legacy
    let leg_row = table
        .find_def("process")
        .expect("process not found (legacy)");
    let leg_req = ShowRequest {
        cached: &cached,
        path: &cpp_path,
        byte_range_start: leg_row.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "process",
        lang_registry: &registry,
        ordinal: None,
    };
    let leg_json =
        show_signature(&leg_req, table.node_kind_of(leg_row)).expect("legacy show_signature");

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
    use forgeql_core::ast::show::{ShowRequest, show_members};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let clauses = Clauses::default();

    // Columnar
    let col_loc = storage
        .resolve_type_symbol("Motor", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("Motor not found (columnar)");
    let col_req = ShowRequest {
        cached: &cached,
        path: &col_loc.path,
        byte_range_start: col_loc.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "Motor",
        lang_registry: &registry,
        ordinal: None,
    };
    let col_json = show_members(&col_req).expect("columnar show_members");

    // Legacy — call the same show_members with the same cached parse + path
    let cpp_path = fixture_path("canonical.cpp");
    let leg_req = ShowRequest {
        cached: &cached,
        path: &cpp_path,
        byte_range_start: 0,
        hint_line: None,
        workspace: &workspace,
        symbol: "Motor",
        lang_registry: &registry,
        ordinal: None,
    };
    let leg_json = show_members(&leg_req).expect("legacy show_members");

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
    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
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
    use forgeql_core::ast::show::{ShowRequest, show_callees};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::workspace::Workspace;

    let (table, _tmp, storage) = single_segment_cpp_overlay();
    let workspace = Workspace::new(fixtures_dir()).expect("workspace");
    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
    let cached = cpp_cached_parse();
    let cpp_path = fixture_path("canonical.cpp");
    let clauses = Clauses::default();

    // Columnar
    let col_loc = storage
        .resolve_body_symbol("caller", &clauses, &fixtures_dir())
        .expect("columnar resolve")
        .expect("caller not found (columnar)");
    let col_req = ShowRequest {
        cached: &cached,
        path: &col_loc.path,
        byte_range_start: col_loc.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "caller",
        lang_registry: &registry,
        ordinal: None,
    };
    let col_json = show_callees(&col_req).expect("columnar show_callees");

    // Legacy
    let leg_row = table.find_def("caller").expect("caller not found (legacy)");
    let leg_req = ShowRequest {
        cached: &cached,
        path: &cpp_path,
        byte_range_start: leg_row.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "caller",
        lang_registry: &registry,
        ordinal: None,
    };
    let leg_json = show_callees(&leg_req).expect("legacy show_callees");

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
    use forgeql_core::ast::show::{ShowRequest, show_body, show_context};
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
    let registry = LanguageRegistry::new(vec![Arc::new(CppLanguage) as Arc<dyn LanguageSupport>]);
    let hash = sha1_of_bytes(&fetched);
    let mut cache = ParseCache::with_capacity(4);
    let cached = cache
        .get_or_parse_with_bytes(hash, &phantom_path, fetched.clone(), &registry)
        .expect("get_or_parse_with_bytes on git-fetched bytes");

    // ── 6. Locate `bar` in the legacy table for a valid byte-range ───────────
    let table = index_fixture(&CppLanguage, "canonical.cpp");
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
    let bare_req = ShowRequest {
        cached: &cached,
        path: &phantom_path,
        byte_range_start: row.byte_range.start,
        hint_line: None,
        workspace: &workspace,
        symbol: "bar",
        lang_registry: &registry,
        ordinal: None,
    };
    let body = show_body(&bare_req, Some(0), &no_enrichment)
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
