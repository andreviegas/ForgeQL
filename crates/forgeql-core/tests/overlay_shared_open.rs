//! Shared-open parity: sessions on one commit share one decode.
//!
//! `warm_or_open` serves the committed overlay and its segment readers from a
//! process-wide cache. These tests exercise the wiring end to end — the cache's
//! own semantics are unit-tested in `storage::columnar::open_cache`, and a unit
//! test cannot see this: it proves the cache honours a caller that holds an
//! entry, while what went wrong here was the real caller holding the entry's
//! *contents* instead, so the entry died between two opens and every session
//! decoded its own copy behind a green suite.

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

use forgeql_core::ir::Clauses;
use forgeql_core::storage::columnar::{BuildInput, ColumnarStorage, OverlayBuilder};
use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
use forgeql_lang_cpp::CppLanguage;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

const COMMIT: &str = "1234567890abcdef1234567890abcdef12345678";

/// One indexed C++ file with its overlay built, ready to open sessions over.
struct Fixture {
    worktree: PathBuf,
    segments_dir: PathBuf,
    overlay_path: PathBuf,
    ctx: ColumnarBuildContext,
    lang_reg: Arc<LanguageRegistry>,
}

/// Returns the fixture and the `TempDir` backing it, which the caller must keep
/// alive for as long as it uses the fixture.
fn fixture() -> (Fixture, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let worktree = tmp.path().join("worktree");
    std::fs::create_dir_all(&worktree).expect("worktree dir");

    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let source = worktree.join("shared.cpp");
    std::fs::write(&source, "void SharedOpenFunc() {}\n").expect("write source");

    let table = index_at_path(&CppLanguage, &source);
    let content_id = build_segment(&table, &source, &segments_dir);
    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(source, content_id);

    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );

    let overlay_path = ctx.overlay_path_for(COMMIT);
    std::fs::create_dir_all(overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", segments_dir.clone(), worktree.clone(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("build overlay");

    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));
    (
        Fixture {
            worktree,
            segments_dir,
            overlay_path,
            ctx,
            lang_reg,
        },
        tmp,
    )
}

impl Fixture {
    fn try_open(&self) -> anyhow::Result<ColumnarStorage> {
        ColumnarStorage::warm_or_open(
            &self.ctx,
            BuildInput {
                table: None,
                prebuilt_segment_map: None,
            },
            self.worktree.clone(),
            COMMIT,
            Arc::clone(&self.lang_reg),
        )
    }

    fn symbol_names(&self, storage: &ColumnarStorage) -> Vec<String> {
        let mut names: Vec<String> = storage
            .find_symbols(&Clauses::default(), &self.worktree)
            .expect("find_symbols")
            .iter()
            .map(|m| m.name.clone())
            .collect();
        names.sort();
        names
    }
}

/// Two sessions on one commit are handed the *same* decode, not equal copies.
///
/// This is the property the whole slice provides, asserted as itself. It also
/// pins the thing that made the cache dead: `ColumnarStorage` must hold the
/// cache entry, because the cache tracks entries weakly and a session holding
/// only the overlay and the readers lets the entry die on the way out of
/// `warm_or_open`.
#[test]
fn two_sessions_on_one_commit_are_handed_the_same_decode() {
    let (fx, _tmp) = fixture();

    let first = fx.try_open().expect("first session opens the overlay");
    let second = fx.try_open().expect("second session opens the overlay");

    assert!(
        Arc::ptr_eq(first.shared_entry(), second.shared_entry()),
        "the second session must be handed the first session's decode"
    );
    assert_eq!(
        fx.symbol_names(&first),
        fx.symbol_names(&second),
        "one shared entry must answer identically for every session holding it"
    );
}

/// The second session does not touch the disk.
///
/// Deleting both the overlay file and the segment tree between the two opens is
/// what makes this sharp: with nothing left to read, the second `warm_or_open`
/// can only succeed from what the first is holding. Both halves go, because
/// removing the overlay alone would leave a test that still passes if the
/// segment readers were re-opened per session — and the readers are the memory
/// this cache exists to save.
///
/// It also pins the behaviour the cache documents as safe: a file removed under
/// a live entry leaves that entry correct, since anything rebuilt at that path
/// is rebuilt with identical content.
#[test]
fn a_second_session_opens_after_the_files_are_gone() {
    let (fx, _tmp) = fixture();

    let first = fx.try_open().expect("first session opens the overlay");
    let expected = fx.symbol_names(&first);
    assert!(
        expected.contains(&"SharedOpenFunc".to_owned()),
        "the first session must see the indexed symbol; got: {expected:?}"
    );

    // `first` stays in scope: a live holder, not any cache-side retention, is
    // what must keep this available.
    std::fs::remove_file(&fx.overlay_path).expect("remove overlay file");
    std::fs::remove_dir_all(&fx.segments_dir).expect("remove segment tree");

    let second = fx
        .try_open()
        .expect("second session is served without reading the deleted files");

    assert_eq!(
        expected,
        fx.symbol_names(&second),
        "a session served from a shared entry must answer as the first one did"
    );
}

/// The cache key names both the overlay file and the segment root.
///
/// Keying on the overlay alone would serve one build context's segment readers
/// to a session whose context resolves segments somewhere else entirely. The
/// unit tests in `open_cache` prove the cache honours a two-part key; this
/// proves the caller actually builds one, and fails if the second half is ever
/// dropped from `ColumnarStorage::open_key`.
#[test]
fn the_open_key_distinguishes_contexts_sharing_an_overlay_directory() {
    let tmp = TempDir::new().expect("tempdir");
    let overlays_dir = tmp.path().join("overlays");

    let context_with_segments_at = |segments: &str| {
        ColumnarBuildContext::new(
            tmp.path().join(segments),
            overlays_dir.clone(),
            "test",
            Arc::new(|b: &[u8]| b.to_vec()),
        )
    };

    let one = context_with_segments_at("segments-one");
    let other = context_with_segments_at("segments-other");
    let overlay_path = one.overlay_path_for(COMMIT);

    assert_eq!(
        overlay_path,
        other.overlay_path_for(COMMIT),
        "the two contexts must agree on the overlay path, or this proves nothing"
    );
    assert_ne!(
        ColumnarStorage::open_key(&one, &overlay_path),
        ColumnarStorage::open_key(&other, &overlay_path),
        "same overlay under a different segment root must be a different entry"
    );
}
