//! A segment the index names but the disk no longer holds fails the open.
//!
//! The vector of segment readers an open produces is indexed positionally by
//! the overlay's own `segment_idx`. Dropping a reader that will not open is
//! therefore not a smaller correct answer: every later index shifts, and rows
//! are served against a different file's reader — same name, same line, a
//! different file's path and content, and a node handle addressing a file the
//! query never named. That is a wrong answer rather than a degraded one, so
//! the open refuses and names both repairs.
//!
//! The state is reachable without anything exotic — an external cleaner, or a
//! reclaim that removed a segment file while leaving the index that names it —
//! though ForgeQL's own `VACUUM` cannot produce it, since it removes whole
//! cache-version directories, index and segments together.

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
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{BuildInput, ColumnarStorage, OverlayBuilder, open_cache};
use forgeql_core::storage::{ColumnarBuildContext, StorageEngine};
use forgeql_lang_cpp::CppLanguage;

const COMMIT: &str = "abcdef0123456789abcdef0123456789abcdef01";

/// The files the fixture indexes, one segment and one function each.
///
/// The overlay sorts its segment table by source path, so these prefixes fix
/// the order the readers are opened in and make "the middle one" well defined.
const SOURCES: [&str; 3] = ["a_alpha.cpp", "b_beta.cpp", "c_gamma.cpp"];

/// A three-segment index on one commit, ready to open sessions over.
struct Fixture {
    worktree: PathBuf,
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

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    for name in SOURCES {
        let source = worktree.join(name);
        let symbol = name.trim_end_matches(".cpp");
        std::fs::write(&source, format!("void {symbol}() {{}}\n")).expect("write source");
        let table = index_at_path(&CppLanguage, &source);
        let content_id = build_segment(&table, &source, &segments_dir);
        let _ = segment_map.insert(source, content_id);
    }

    let ctx = ColumnarBuildContext::new(
        segments_dir.clone(),
        overlays_dir,
        "test",
        Arc::new(|b: &[u8]| b.to_vec()),
    );

    let overlay_path = ctx.overlay_path_for(COMMIT);
    std::fs::create_dir_all(overlay_path.parent().unwrap()).expect("overlay parent");
    OverlayBuilder::new("test", segments_dir, worktree.clone(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("build overlay");

    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));

    (
        Fixture {
            worktree,
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

    /// Every function row as `(name, the file it is reported under)`, sorted.
    ///
    /// The pairing is the whole point: a shifted reader vector keeps every name
    /// and line and changes only which file they are attributed to.
    fn name_and_file(&self, storage: &ColumnarStorage) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = storage
            .find_symbols(&Clauses::default(), &self.worktree)
            .expect("find_symbols")
            .iter()
            .filter(|m| m.fql_kind.as_deref() == Some("function"))
            .map(|m| {
                let file = m
                    .path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default();
                (m.name.clone(), file)
            })
            .collect();
        pairs.sort();
        pairs
    }

    /// The file holding `source`'s segment, named the way the engine names it —
    /// read back out of the overlay rather than recomputed here, so it
    /// cannot drift from the path the open will actually look at.
    fn segment_file(&self, source: &str) -> PathBuf {
        let overlay = Overlay::open(&self.overlay_path).expect("open overlay");
        let meta = overlay
            .segments()
            .iter()
            .find(|m| m.source_path == std::path::Path::new(source))
            .expect("the overlay lists this segment");
        self.ctx
            .segment_path_for(&meta.source_path, &meta.hex_content_id)
    }

    /// Whether some caller still holds this commit's decode.
    fn is_cached(&self) -> bool {
        open_cache::peek(&ColumnarStorage::open_key(&self.ctx, &self.overlay_path)).is_some()
    }

    /// Opens, requiring the open to be refused, and returns the refusal.
    ///
    /// `Result::expect_err` needs `Debug` on the success type and
    /// `ColumnarStorage` has none, so match it out by hand.
    fn expect_refusal(&self) -> anyhow::Error {
        match self.try_open() {
            Ok(_) => panic!("an index missing a segment must not open"),
            Err(e) => e,
        }
    }
}

/// The expected pairing when nothing is missing.
fn whole_index() -> Vec<(String, String)> {
    SOURCES
        .iter()
        .map(|f| ((*f).trim_end_matches(".cpp").to_owned(), (*f).to_owned()))
        .collect()
}

/// A segment the overlay names but the disk no longer holds fails the open.
#[test]
fn a_missing_segment_fails_the_open_instead_of_shifting_every_later_row() {
    let (fx, _tmp) = fixture();

    // Control: with every segment present, each name is reported under the file
    // it was indexed from. Pin that pairing before breaking it — it is what a
    // shifted reader vector destroys while leaving names and lines intact.
    let whole = fx.try_open().expect("a complete index opens");
    assert_eq!(
        fx.name_and_file(&whole),
        whole_index(),
        "each symbol must be reported under its own file"
    );

    let missing = fx.segment_file(SOURCES[1]);
    drop(whole);

    // A later caller is handed the decode an earlier one still holds, so the
    // control's entry has to be gone before a deleted file can be observed at
    // all. Assert that precondition rather than assuming it: the rest of this
    // test is vacuous if the re-open below is served from memory.
    assert!(
        !fx.is_cached(),
        "dropping the only holder must release the cached decode"
    );

    // Remove the middle segment. Every reader after it shifts down by one if
    // the open tolerates the gap.
    std::fs::remove_file(&missing).expect("remove the segment file");

    let err = fx.expect_refusal();
    let msg = format!("{err:#}");
    assert!(
        msg.contains(SOURCES[1]),
        "the error must name the file whose segment is gone: {msg}"
    );
    assert!(
        msg.contains(&*fx.overlay_path.to_string_lossy()),
        "the error must name the index to remove if a rebuild is wanted, since that \
         is the repair the operator has to reach for by hand: {msg}"
    );
}

/// A refused open caches nothing, and the repair takes effect on the next open.
///
/// Both halves matter. Caching a refusal would make one missing file poison
/// every later caller on that commit, and a refusal that outlived the repair
/// would force a restart to recover from a re-index.
#[test]
fn a_refused_open_caches_nothing_and_the_repair_takes_effect_at_once() {
    let (fx, tmp) = fixture();

    let missing = fx.segment_file(SOURCES[0]);
    let stash = tmp.path().join("stashed-segment");
    std::fs::rename(&missing, &stash).expect("move the segment aside");

    let _ = fx.expect_refusal();
    assert!(
        !fx.is_cached(),
        "a refused open must leave nothing for the next caller to be handed"
    );

    std::fs::rename(&stash, &missing).expect("put the segment back");

    let repaired = fx
        .try_open()
        .expect("the index opens once the segment is back");
    assert_eq!(
        fx.name_and_file(&repaired),
        whole_index(),
        "the repaired index must answer over every file again"
    );
}

/// The overlay survives the refusal, so re-indexing has something to repair.
///
/// The unreadable-overlay path deletes the overlay and rebuilds; this one must
/// not, because the rebuild drops a segment it cannot read and would answer
/// from a smaller index without saying so — an unannounced absence in place of
/// a wrong answer, which is the worse of the two.
#[test]
fn a_refused_open_leaves_the_overlay_in_place() {
    let (fx, _tmp) = fixture();

    std::fs::remove_file(fx.segment_file(SOURCES[2])).expect("remove the segment file");

    let _ = fx.expect_refusal();
    assert!(
        fx.overlay_path.exists(),
        "the overlay itself read fine and must not be deleted as if it were corrupt"
    );
}
