//! An `ORDER BY … LIMIT` page must hold `LIMIT` distinct rows.
//!
//! Rows agreeing on `(name, fql_kind, path, line)` are one answer row, and
//! every place that stops reading early used to stop before they were
//! collapsed. Where a group of them sorted to the front, the window that was
//! read filled with rows that later merged into one and the page came back
//! short, the distinct rows that belonged in it having already been passed
//! over. The fixture below is that shape: `DUPS` rows agreeing on every key
//! field, sorting ahead of `DISTINCT` rows that do not.
//!
//! **This case does not exercise the mechanism it was written for, and that
//! is worth keeping.** It was recorded as reproducing the running top-K trim
//! in `materialize_all`. It never reaches it: `ORDER BY name ASC` with no
//! `WHERE` and an empty dirty overlay is answered by the name-index stream,
//! which returns `LIMIT + OFFSET` rows and stops, so the trim is never armed
//! and `materialize_all` is never called. Making the trim collapse first left
//! this red, unchanged, and only then did varying the ordering field separate
//! the two. Three places carried one defect and a working reproduction of it
//! named the wrong one — which is the shape most likely to be believed
//! without checking.
//!
//! What makes it pass is the stream declining a page it cannot fill: a stream
//! that stopped at its own limit and then lost rows to a collapse hands the
//! query back to the pipeline, which reads every segment. The trim and the
//! per-segment bounded choice collapse before they shed as well, and the
//! sibling case below covers the trim directly by ordering on a field no
//! stream serves.
//!
//! The last two cases are about the one workspace shape in which collapsing
//! first is still not enough. Two segments built from one source path hold
//! rows that are duplicates of each other, and neither segment can see that
//! from inside itself — so a row one of them discards on rank may be the row
//! the other's copy was going to merge into, and a row it counts as shed may
//! be counted as shed again by its neighbour. There the engine does not shed
//! at all. Those cases build that shape and ask what the count comes back as.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forgeql_core::ast::lang::LanguageRegistry;
use forgeql_core::ir::{Clauses, OrderBy, SortDirection};
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{
    ColumnarStorage, OverlayBuilder, SegmentBuilder, SegmentReader, SymbolRow,
};
use tempfile::TempDir;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

fn vp() -> String {
    format!("test-v{}", forgeql_core::storage::columnar::ENRICH_VER)
}

fn seg_path(base: &Path, source_path: &Path, hex: &str) -> PathBuf {
    base.join(vp())
        .join(forgeql_core::storage::columnar::segment_rel_path(
            source_path,
            hex,
        ))
}

/// Where the collapsing group sits in the sort order.
///
/// The two positions test different machinery, and a fixture that only builds
/// one of them cannot tell them apart. With the duplicates at the **head**, a
/// page cut from the front is mostly one row wearing several hats, so a
/// bounded chooser that sheds before collapsing loses distinct rows it should
/// have kept — and a name stream that pages the head hands back short. With
/// them in the **tail**, a page from the front is already whole and already
/// distinct: nothing collapses, nothing is short, and the only thing left that
/// can be wrong is the count.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DupPosition {
    /// Duplicates sort first, ahead of every distinct row.
    Head,
    /// Duplicates sort last, behind every distinct row — and each segment's
    /// distinct rows are its own, so only the tail key is shared between them.
    ///
    /// That combination is what leaves a head-cut page whole: were both
    /// segments to hold the same distinct rows, the page would collapse across
    /// them and hand back short whatever the count said.
    Tail,
}

/// Emit the fixture's rows: `dups` rows agreeing on every field of FIND's
/// dedupe key, so Stage 4 collapses them to one, and `distinct` rows agreeing
/// with nothing. `pos` decides which group sorts first on both `name` and
/// `line`.
fn emit_fixture_rows(
    builder: &mut SegmentBuilder,
    dups: usize,
    distinct: usize,
    pos: DupPosition,
    seg_tag: &str,
) {
    let tail_prefix = format!("aaa_{seg_tag}");
    let (dup_name, dup_line, distinct_prefix, distinct_line) = match pos {
        DupPosition::Head => ("aaa_duplicate", 1u32, "zzz_distinct", 100u32),
        DupPosition::Tail => ("zzz_duplicate", 900u32, tail_prefix.as_str(), 100u32),
    };
    let emit_dups = |builder: &mut SegmentBuilder| {
        for i in 0..dups {
            let _ = builder.emit_row(SymbolRow {
                name: dup_name,
                fql_kind: "function",
                language: "cpp",
                line: dup_line,
                byte_start: u32::try_from(i).unwrap(),
                byte_end: u32::try_from(i + 1).unwrap(),
                usages_count: 0,
            });
        }
    };
    let emit_distinct = |builder: &mut SegmentBuilder| {
        for i in 0..distinct {
            let name = format!("{distinct_prefix}_{i:03}");
            let _ = builder.emit_row(SymbolRow {
                name: &name,
                fql_kind: "function",
                language: "cpp",
                line: distinct_line + u32::try_from(i).unwrap(),
                byte_start: u32::try_from(1000 + i).unwrap(),
                byte_end: u32::try_from(1001 + i).unwrap(),
                usages_count: 0,
            });
        }
    };
    match pos {
        DupPosition::Head => {
            emit_dups(builder);
            emit_distinct(builder);
        }
        DupPosition::Tail => {
            emit_distinct(builder);
            emit_dups(builder);
        }
    }
}

/// Flush one segment holding the fixture's rows, under a content ID made of
/// `fill` bytes, and hand that ID back.
fn flush_segment(
    segments_dir: &Path,
    fill: u8,
    dups: usize,
    distinct: usize,
    pos: DupPosition,
    seg_tag: &str,
) -> Vec<u8> {
    let content_id: Vec<u8> = vec![fill; 8];
    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let mut builder = SegmentBuilder::new("test", &content_id);
    emit_fixture_rows(&mut builder, dups, distinct, pos, seg_tag);
    builder
        .flush(&seg_path(segments_dir, Path::new("canonical.cpp"), &hex))
        .expect("segment flush");
    content_id
}

fn overlay_path(tmp: &TempDir) -> PathBuf {
    tmp.path().join("overlays").join("test").join("trim.bin")
}

/// Build the overlay over `segment_map` and open a storage on it.
fn open_storage(
    tmp: &TempDir,
    segments_dir: &Path,
    segment_map: HashMap<PathBuf, Vec<u8>>,
) -> ColumnarStorage {
    OverlayBuilder::new(
        "test",
        segments_dir.to_path_buf(),
        fixtures_dir(),
        segment_map,
    )
    .build_and_persist(&overlay_path(tmp))
    .expect("overlay build");

    let overlay = Overlay::open(&overlay_path(tmp)).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_path(segments_dir, &m.source_path, &m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    ColumnarStorage::new_unshared(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    )
}

/// A one-segment index whose lowest-sorting rows are all the same row.
fn storage_with_duplicate_heavy_head(dups: usize, distinct: usize) -> (TempDir, ColumnarStorage) {
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let content_id = flush_segment(
        &segments_dir,
        0x7A,
        dups,
        distinct,
        DupPosition::Head,
        "one",
    );

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(fixtures_dir().join("canonical.cpp"), content_id);

    let storage = open_storage(&tmp, &segments_dir, segment_map);
    (tmp, storage)
}

/// The same rows again, in TWO segments that report the SAME source path.
///
/// A segment is registered under the path of its source, and the overlay
/// builder relativises that against the worktree root before it becomes the
/// segment's `source_path` — a key that is already relative passes through
/// untouched. So the two entries below are distinct keys naming one file, and
/// the overlay comes out holding two segments whose `source_path` is
/// `canonical.cpp`. That is the state `Overlay::has_duplicate_paths` reports,
/// and in it every row of the second segment is a duplicate of a row of the
/// first, which no single segment can see.
fn storage_with_one_path_in_two_segments(
    dups: usize,
    distinct: usize,
    pos: DupPosition,
) -> (TempDir, ColumnarStorage) {
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let first = flush_segment(&segments_dir, 0x7A, dups, distinct, pos, "one");
    let second = flush_segment(&segments_dir, 0x7B, dups, distinct, pos, "two");

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(fixtures_dir().join("canonical.cpp"), first);
    let _ = segment_map.insert(PathBuf::from("canonical.cpp"), second);

    let storage = open_storage(&tmp, &segments_dir, segment_map);
    (tmp, storage)
}

const DUPS: usize = 6;
const DISTINCT: usize = 6;
const LIMIT: usize = 2;

/// The control, and the reason the case below is a defect rather than a matter
/// of opinion: unpaged, the same index answers with far more distinct rows than
/// the page asks for. A fixture drift that stopped producing them would fail
/// here instead of leaving the case below asserting a number that had quietly
/// become unreachable.
#[test]
fn the_index_holds_more_distinct_rows_than_a_page_asks_for() {
    let (_tmp, storage) = storage_with_duplicate_heavy_head(DUPS, DISTINCT);

    let rows = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("find_symbols");

    assert_eq!(
        rows.len(),
        DISTINCT + 1,
        "the {DUPS} duplicates collapse to one and the {DISTINCT} distinct rows stay"
    );
    assert!(
        rows.len() > LIMIT,
        "the fixture must hold more distinct rows than the page asks for"
    );
}

/// A page is a window onto the answer, so asking for `LIMIT` rows when far more
/// than `LIMIT` distinct rows match has to return `LIMIT` of them.
#[test]
fn order_by_limit_returns_a_full_page_of_distinct_rows() {
    let (_tmp, storage) = storage_with_duplicate_heavy_head(DUPS, DISTINCT);

    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(LIMIT),
        ..Clauses::default()
    };
    let rows = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("find_symbols");
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();

    assert_eq!(
        rows.len(),
        LIMIT,
        "ORDER BY name LIMIT {LIMIT} returned a short page: {names:?}"
    );
    let unique: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique.len(),
        LIMIT,
        "the page must hold {LIMIT} distinct rows, got {names:?}"
    );
}

/// The same shape ordered on a field no name stream serves, so it reaches the
/// pipeline and the two trims that shed on rank.
///
/// The duplicates share line 1 and every distinct row is at 100 or above, so
/// an ascending order on `line` puts the group that collapses at the front
/// exactly as an ascending order on `name` does — but `ORDER BY line` has no
/// index stream behind it, so the query is answered by `materialize_all`, the
/// per-segment bounded choice picks this segment's contribution from its row
/// IDs, and the running trim cuts the accumulated set. Both collapse first.
/// Remove either collapse and the retained window fills with rows that are
/// one row, and this page comes back holding one.
#[test]
fn ordering_on_a_field_no_stream_serves_still_returns_a_full_page() {
    let (_tmp, storage) = storage_with_duplicate_heavy_head(DUPS, DISTINCT);

    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(LIMIT),
        ..Clauses::default()
    };
    let page = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("find_symbols");
    let names: Vec<&str> = page.iter().map(|r| r.name.as_str()).collect();

    assert_eq!(
        page.rows.len(),
        LIMIT,
        "ORDER BY line LIMIT {LIMIT} returned a short page: {names:?}"
    );
    let unique: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique.len(),
        LIMIT,
        "the page must hold {LIMIT} distinct rows, got {names:?}"
    );
    assert_eq!(
        page.total,
        DISTINCT + 1,
        "the count beside the page is the size of the answer, so the rows the \
         trim shed on rank are in it and the ones that collapsed are not"
    );
}

/// The control for the two-segment fixture, and what makes the case below
/// about the engine rather than about the fixture: the overlay really does
/// hold two segments naming one source path, and the answer over them is the
/// answer over one, because every row of the second collapses into a row of
/// the first.
#[test]
fn two_segments_can_report_one_source_path() {
    let (tmp, storage) = storage_with_one_path_in_two_segments(DUPS, DISTINCT, DupPosition::Head);

    let overlay = Overlay::open(&overlay_path(&tmp)).expect("Overlay::open");
    assert_eq!(
        overlay.segments().len(),
        2,
        "the fixture must build two segments"
    );
    assert!(
        overlay.has_duplicate_paths(),
        "the fixture must reach the overlay state the disarm keys on"
    );

    let page = storage
        .find_symbols(&Clauses::default(), Path::new("."))
        .expect("find_symbols");
    assert_eq!(
        page.rows.len(),
        DISTINCT + 1,
        "two segments over one path hold one answer, not two"
    );
    assert_eq!(page.total, DISTINCT + 1, "and the count agrees with it");
}

/// Two segments on one path is the one shape in which a segment's own collapse
/// is not the whole collapse: a row it discards on rank can be the row another
/// segment's copy was going to merge into, and a row it counts as shed can be
/// a row another segment counts as shed as well. So nothing may shed there at
/// all — `trim_budget` returns `None` when the overlay reports duplicate
/// paths, and both the per-segment bounded choice and the running trim take
/// that one value rather than deriving their own.
///
/// Measured, with the edit that produced the number: delete those three lines
/// from `trim_budget`, leaving its body as `Self::topk_trim_for(clauses)` (and
/// `#[allow(clippy::unused_self)]` on it so the lint gate still reaches the
/// tests), and this case reports `total: 10` against an answer of 7. Each
/// segment keeps `topk_keep(LIMIT)` of its own seven distinct rows and counts
/// the remaining three as shed, and the two segments were holding copies of
/// each other's rows. Nothing else in the tree changes: it is the only case
/// that notices, because the unit tests in `fast_paths/tests.rs` pass
/// `topk_trim_for` to the bounded choice directly, which is the ungated value.
///
/// The mirror direction is not pinned, and cannot be pinned by asking for an
/// answer: disarm unconditionally and this still reports 7, because refusing
/// to shed only ever keeps rows the answer already holds. What that costs is
/// peak memory on a large scan, which no assertion on a page can see.
#[test]
fn duplicate_paths_disarm_the_shedders_so_the_count_stays_honest() {
    let (_tmp, storage) = storage_with_one_path_in_two_segments(DUPS, DISTINCT, DupPosition::Head);

    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(LIMIT),
        ..Clauses::default()
    };
    let page = storage
        .find_symbols(&clauses, Path::new("."))
        .expect("find_symbols");
    let names: Vec<&str> = page.iter().map(|r| r.name.as_str()).collect();

    assert_eq!(
        page.rows.len(),
        LIMIT,
        "ORDER BY line LIMIT {LIMIT} returned a short page: {names:?}"
    );
    let unique: HashSet<&str> = names.iter().copied().collect();
    assert_eq!(
        unique.len(),
        LIMIT,
        "the page must hold {LIMIT} distinct rows, got {names:?}"
    );
    assert_eq!(
        page.total,
        DISTINCT + 1,
        "the count is the size of the answer, and a row shed once by every \
         segment holding a copy of it is counted more times than the answer \
         holds rows"
    );
}

/// The name streams decline the duplicated-path shape before they stream.
///
/// Their `total` is summed from the per-segment deduplicated counts stored at
/// overlay build time, and two segments built from one source path hold rows
/// that are duplicates of each other, which those per-segment counts cannot
/// see — the sum here would be double the answer. So the `ORDER BY name`
/// shapes and the bare `LIMIT` shape both hand this workspace to the
/// pipeline, whose collapse produces the count directly. Serving the stream
/// anyway fails this test with a doubled total; reporting the streamed page
/// as the total fails it with `total == LIMIT`.
///
/// **The duplicates sort in the tail here, and that is what makes this about
/// the gate.** With them at the head a page cut from the front is mostly one
/// row repeated, so it collapses short and the stream hands back under the
/// whole-or-declined rule — a different mechanism, which catches the shape
/// before the duplicate-path gate is consulted at all. Measured: with a
/// head-position fixture, deleting the `has_duplicate_paths` check from
/// `try_order_by_name_fast_paths` left this test and the whole tree green. In
/// the tail position the page comes back whole and distinct, nothing collapses
/// it, and the gate is the only thing standing between the caller and a total
/// of `2 * (DISTINCT + 1)`.
#[test]
fn duplicate_paths_decline_the_name_streams_so_the_total_stays_honest() {
    let (_tmp, storage) = storage_with_one_path_in_two_segments(DUPS, DISTINCT, DupPosition::Tail);

    let by_name = Clauses {
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(LIMIT),
        ..Clauses::default()
    };
    let page = storage
        .find_symbols(&by_name, Path::new("."))
        .expect("find_symbols ORDER BY name");
    // Each segment holds its own DISTINCT rows plus the one shared tail key,
    // so the collapsed answer is 2 * DISTINCT + 1 while the stored per-segment
    // counts sum to 2 * (DISTINCT + 1) — one more, because each segment counts
    // the shared key it cannot see the other holding.
    assert_eq!(
        page.total,
        2 * DISTINCT + 1,
        "ORDER BY name on a duplicated path must answer with the collapsed \
         answer's size — neither the doubled per-segment sum nor the page's"
    );

    let bare = Clauses {
        limit: Some(LIMIT),
        ..Clauses::default()
    };
    let bare_page = storage
        .find_symbols(&bare, Path::new("."))
        .expect("find_symbols bare LIMIT");
    assert_eq!(
        bare_page.total,
        2 * DISTINCT + 1,
        "a bare LIMIT declines the same way on this shape"
    );
    assert_eq!(bare_page.rows.len(), LIMIT, "the page itself is still full");
}
