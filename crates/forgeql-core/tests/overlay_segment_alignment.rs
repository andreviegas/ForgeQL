//! A segment the index names but the disk no longer holds never yields rows.
//!
//! The vector of segment readers an open produces is indexed positionally by
//! the overlay's own `segment_idx`. Dropping a reader that will not open is
//! therefore not a smaller correct answer: every later index shifts, and rows
//! are served against a different file's reader — same name, same line, a
//! different file's path and content, and a node handle addressing a file the
//! query never named. That is a wrong answer rather than a degraded one.
//!
//! What happens instead depends on what the caller brought. A rebuild that
//! shadow-writes from a merged symbol table writes every segment that is not
//! already valid, so it regenerates the missing one and the index is repaired.
//! A rebuild that only assembles the segments already on disk cannot, and
//! neither can a caller that brought nothing: those refuse and name the file.
//! Either way a readable overlay is left where it is.
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
    /// What the inline rebuild path would be handed: the segments as they were
    /// when the overlay was built.
    segment_map: HashMap<PathBuf, Vec<u8>>,
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
    OverlayBuilder::new("test", segments_dir, worktree.clone(), segment_map.clone())
        .build_and_persist(&overlay_path)
        .expect("build overlay");

    let lang_reg = Arc::new(LanguageRegistry::new(vec![Arc::new(CppLanguage)]));

    (
        Fixture {
            worktree,
            overlay_path,
            ctx,
            lang_reg,
            segment_map,
        },
        tmp,
    )
}

/// A caller carrying nothing to rebuild from.
const fn no_rebuild_source() -> BuildInput<'static> {
    BuildInput {
        table: None,
        prebuilt_segment_map: None,
    }
}

/// One `SymbolTable` holding every file, which is what a shadow-writing rebuild
/// takes as its source — and, unlike the per-file tables the fixture builds, is
/// able to write a segment that is missing.
fn merged_table(worktree: &std::path::Path) -> SymbolTable {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&CppLanguage.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();

    let mut table = SymbolTable::default();
    for name in SOURCES {
        let path = worktree.join(name);
        let mut ctx = IndexContext {
            path: &path,
            language: &CppLanguage,
            enrichers: &enrichers,
            macro_table: None,
            ordinal_remapper: None,
            table: &mut table,
            workspace_root: Some(worktree),
        };
        let _ = index_file(&mut parser, &mut ctx, None).expect("index_file should succeed");
    }
    table
}

impl Fixture {
    fn try_open(&self, input: BuildInput<'_>) -> anyhow::Result<ColumnarStorage> {
        ColumnarStorage::warm_or_open(
            &self.ctx,
            input,
            self.worktree.clone(),
            COMMIT,
            Arc::clone(&self.lang_reg),
        )
    }

    /// Opens, requiring the open to be refused, and returns the refusal.
    ///
    /// `Result::expect_err` needs `Debug` on the success type and
    /// `ColumnarStorage` has none, so match it out by hand.
    fn expect_refusal(&self, input: BuildInput<'_>) -> anyhow::Error {
        match self.try_open(input) {
            Ok(_) => panic!("an index missing a segment must not open"),
            Err(e) => e,
        }
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
}

/// The expected pairing when nothing is missing.
fn whole_index() -> Vec<(String, String)> {
    SOURCES
        .iter()
        .map(|f| ((*f).trim_end_matches(".cpp").to_owned(), (*f).to_owned()))
        .collect()
}

/// A segment the overlay names but the disk no longer holds is never read past.
#[test]
fn a_missing_segment_fails_the_open_instead_of_shifting_every_later_row() {
    let (fx, _tmp) = fixture();

    // Control: with every segment present, each name is reported under the file
    // it was indexed from. Pin that pairing before breaking it — it is what a
    // shifted reader vector destroys while leaving names and lines intact.
    let whole = fx
        .try_open(no_rebuild_source())
        .expect("a complete index opens");
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

    let err = fx.expect_refusal(no_rebuild_source());
    let msg = format!("{err:#}");
    assert!(
        msg.contains(SOURCES[1]),
        "the error must name the file whose segment is gone: {msg}"
    );
    assert!(
        msg.contains(&*fx.overlay_path.to_string_lossy()),
        "the error must name the index it is talking about: {msg}"
    );
}

/// A missing segment is regenerated rather than refused when the caller carries
/// something that can write it again.
///
/// Refusing here would be the safe-looking answer and the wrong one: the
/// guarantee is that a wrong answer is never served, and a rebuild that
/// restores the segment honours it while leaving the index usable.
#[test]
fn a_missing_segment_is_regenerated_when_a_symbol_table_can_write_it() {
    let (fx, _tmp) = fixture();

    let missing = fx.segment_file(SOURCES[1]);
    std::fs::remove_file(&missing).expect("remove the segment file");

    let table = merged_table(&fx.worktree);
    let repaired = fx
        .try_open(BuildInput {
            table: Some(&table),
            prebuilt_segment_map: None,
        })
        .expect("a rebuild that can regenerate the segment must not refuse");

    // The rebuild recomputes the content id with the build context's own hash
    // function, so the regenerated segment is content-addressed differently
    // from the one the fixture wrote by hand and lands at a different path.
    // Ask the repaired index where it keeps that file's segment now rather than
    // pinning the fixture's arithmetic.
    assert!(
        fx.segment_file(SOURCES[1]).exists(),
        "the rebuild must write the missing segment back, not route around it"
    );
    assert_eq!(
        fx.name_and_file(&repaired),
        whole_index(),
        "the repaired index must answer over every file, each under its own name"
    );
}

/// A rebuild that runs and does not write the segment back is still refused.
///
/// Authorising a rebuild is not the same as the rebuild working. It skips a
/// segment whose first four bytes still look right even when opening it fails,
/// a write that fails leaves nothing behind, and a source it cannot read drops
/// out — and the assembly step that ends every rebuild then omits that file.
/// Without the check that follows the rebuild, the open succeeds over an index
/// one file smaller and says nothing at all. This drives the last of those
/// three; the check does not care which one it was.
#[test]
fn a_rebuild_that_does_not_write_the_segment_back_is_still_refused() {
    let (fx, _tmp) = fixture();

    // Index while the file is still there, then take away both its segment and
    // the file itself. The rebuild is authorised — a symbol table is available —
    // and it runs, and it cannot write the segment, because the source it would
    // write it from is gone. The rebuilt index simply does not mention that file
    // any more.
    let table = merged_table(&fx.worktree);
    std::fs::remove_file(fx.segment_file(SOURCES[1])).expect("remove the segment file");
    std::fs::remove_file(fx.worktree.join(SOURCES[1])).expect("remove the source file");

    let err = fx.expect_refusal(BuildInput {
        table: Some(&table),
        prebuilt_segment_map: None,
    });

    let msg = format!("{err:#}");
    assert!(
        msg.contains(SOURCES[1]),
        "the refusal must name the file whose segment is still not there: {msg}"
    );
    assert!(
        msg.contains("a rebuild ran and did not write it back"),
        "and must say a rebuild was tried, not that none was available: {msg}"
    );
}

/// A rebuild that could only drop the segment refuses instead of running.
///
/// The inline path assembles the overlay from the segments already on disk, so
/// it cannot write the one that is gone — it would leave it out and produce a
/// smaller index that never says it is smaller. That is worse than the wrong
/// answer this whole suite is about, so having *a* rebuild available is not
/// enough; it has to be one that regenerates.
#[test]
fn a_missing_segment_is_refused_when_the_rebuild_would_only_drop_it() {
    let (fx, _tmp) = fixture();

    let missing = fx.segment_file(SOURCES[1]);
    std::fs::remove_file(&missing).expect("remove the segment file");

    // Both sources are present, which is what a cold attach carries once its
    // build has written the segments inline. The inline map wins in
    // `build_overlay`, so the rebuild that would actually run is the one that
    // cannot write the segment back — having *a* rebuild available is not
    // enough, it has to be one that regenerates.
    let table = merged_table(&fx.worktree);
    let err = fx.expect_refusal(BuildInput {
        table: Some(&table),
        prebuilt_segment_map: Some(fx.segment_map.clone()),
    });

    let msg = format!("{err:#}");
    assert!(
        msg.contains(SOURCES[1]),
        "the error must name the file whose segment is gone: {msg}"
    );
    assert!(
        !msg.contains("a rebuild ran and did not write it back"),
        "this must be the refusal that runs *no* rebuild — the post-condition's \
         refusal, raised after a rebuild that produced nothing, leaves the same \
         outcome behind and would hide the decision entirely: {msg}"
    );
    assert!(
        !missing.exists(),
        "and the segment is still gone, since no rebuild here could have written it"
    );
    assert!(
        fx.overlay_path.exists(),
        "and it must not take the readable index down with it"
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

    let _ = fx.expect_refusal(no_rebuild_source());
    assert!(
        !fx.is_cached(),
        "a refused open must leave nothing for the next caller to be handed"
    );

    std::fs::rename(&stash, &missing).expect("put the segment back");

    let repaired = fx
        .try_open(no_rebuild_source())
        .expect("the index opens once the segment is back");
    assert_eq!(
        fx.name_and_file(&repaired),
        whole_index(),
        "the repaired index must answer over every file again"
    );
}

/// The overlay survives the refusal, so the repair has something to repair.
///
/// The unreadable-overlay path deletes the overlay and rebuilds; this one must
/// not. Removing a readable index cannot be undone, and here there is nothing
/// to rebuild it from — deleting it would turn a refusal into a loss.
#[test]
fn a_refused_open_leaves_the_overlay_in_place() {
    let (fx, _tmp) = fixture();

    std::fs::remove_file(fx.segment_file(SOURCES[2])).expect("remove the segment file");

    let _ = fx.expect_refusal(no_rebuild_source());
    assert!(
        fx.overlay_path.exists(),
        "the overlay itself read fine and must not be deleted as if it were corrupt"
    );
}
