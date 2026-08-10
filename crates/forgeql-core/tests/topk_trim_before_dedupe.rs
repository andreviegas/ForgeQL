//! The running top-K trim sheds rows before duplicates are collapsed.
//!
//! `materialize_all` trims its working set to `LIMIT * 2` as soon as it exceeds
//! `LIMIT * TOPK_OVER_FETCH`, ordering by the query's own comparator. Stage 4
//! collapses duplicates on `(name, fql_kind, path, line)` afterwards, and the
//! final top-K runs after that. So rows the trim discarded are gone before
//! anything knows how many of the rows it kept were the same row.
//!
//! Where a group of duplicates sorts to the front, the retained window can be
//! filled by rows that later collapse into one, leaving a page shorter than the
//! `LIMIT` while distinct rows that belonged in the answer were already shed.
//!
//! **The failing case is `#[ignore]`d, which is weaker than this repo's usual
//! marker for an open defect.** A known defect is normally pinned by an
//! `expect_fail` case in the golden suite, which the gate prints as an open
//! defect rather than silently skipping. The shape itself is production
//! reachable — `crates/forgeql-core/src/storage/columnar/overlay/parse.rs` line
//! 482 carries four rows agreeing on every field of the dedupe key, already
//! `2 * LIMIT` for a `LIMIT` of 2 — but a golden case also needs a query whose
//! candidate set puts such a group at the HEAD of the retained window, and no
//! corpus query is known that does. Building the rows directly is also what
//! makes the counts exact, so the reproduction lives here and announces itself
//! in its `ignore` reason instead. Removing that attribute is the promotion.

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

/// A one-segment index whose lowest-sorting rows are all the same row.
///
/// The `dups` rows are byte-distinct nodes agreeing on every field of FIND's
/// dedupe key, so Stage 4 collapses them to one; their name sorts before every
/// distinct row, so an ascending ORDER BY puts them at the front of the trim's
/// retained window.
fn storage_with_duplicate_heavy_head(dups: usize, distinct: usize) -> (TempDir, ColumnarStorage) {
    let src = fixtures_dir().join("canonical.cpp");
    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");

    let content_id: Vec<u8> = vec![0x7A; 8];
    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let mut builder = SegmentBuilder::new("test", &content_id);
    for i in 0..dups {
        let _ = builder.emit_row(SymbolRow {
            name: "aaa_duplicate",
            fql_kind: "function",
            language: "cpp",
            line: 1,
            byte_start: u32::try_from(i).unwrap(),
            byte_end: u32::try_from(i + 1).unwrap(),
            usages_count: 0,
        });
    }
    for i in 0..distinct {
        let name = format!("zzz_distinct_{i:03}");
        let _ = builder.emit_row(SymbolRow {
            name: &name,
            fql_kind: "function",
            language: "cpp",
            line: u32::try_from(100 + i).unwrap(),
            byte_start: u32::try_from(1000 + i).unwrap(),
            byte_end: u32::try_from(1001 + i).unwrap(),
            usages_count: 0,
        });
    }
    builder
        .flush(&seg_path(&segments_dir, Path::new("canonical.cpp"), &hex))
        .expect("segment flush");

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(src, content_id);
    let overlay_path = tmp.path().join("overlays").join("test").join("trim.bin");
    OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
        .build_and_persist(&overlay_path)
        .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segs: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&seg_path(&segments_dir, &m.source_path, &m.hex_content_id))
                    .expect("open segment"),
            )
        })
        .collect();
    let storage = ColumnarStorage::new_unshared(
        fixtures_dir(),
        segs,
        overlay,
        Arc::new(LanguageRegistry::new(vec![])),
    );
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
#[ignore = "reproduces an open defect and fails: the running top-K trim sheds rows before Stage 4 collapses duplicates, so an ORDER BY + LIMIT page can come back shorter than its LIMIT while distinct rows that belonged in the answer were already discarded. Remove this attribute once the pipeline dedupes over row IDs before the top-K"]
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
