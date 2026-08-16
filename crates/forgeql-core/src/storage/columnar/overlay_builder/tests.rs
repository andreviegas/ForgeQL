//! Unit tests for the enrichment-bitmap step of [`OverlayBuilder`].
//!
//! These drive `step55_build_enrich_bitmaps` over real flushed segments and
//! decode the blob it hands back.  That is one level below the engine: the
//! step's single call site in `build_and_persist` is not exercised here, so
//! deleting that line would leave this file green.  The wiring is pinned
//! instead by every integration test that builds an overlay
//! (`tests/overlay_*_parity.rs`), and by the overlay checksum any change to
//! the collector's shape is measured against.
//!
//! What only this level can see is the budget rule.  A field carrying more
//! than `overlay_budget` distinct values is dropped from the overlay WHOLE —
//! including the keys that segments read before the field ran over had already
//! contributed — and a query's answer does not change when it is, because a
//! pruned field simply falls back to a full scan.  So the rule is invisible
//! from the DSL, and a collector that got it wrong would be caught by nothing
//! short of comparing whole overlays byte for byte.

use super::*;
use crate::storage::columnar::segment_builder::{SegmentBuilder, SymbolRow};

/// Rows per segment in the shared fixture.
const SEG_ROWS: usize = 40;

/// Flush a segment whose row `i` carries `values[i]` of each named column, and
/// open it.  Names and lines are distinct per row so `step45_dedup_segments`
/// keeps every row canonical.
fn segment_with_columns(dir: &Path, stem: &str, columns: &[(&str, Vec<String>)]) -> SegmentReader {
    let rows = columns.first().map_or(0, |(_, values)| values.len());
    let path = dir.join(format!("{stem}.fqsf"));
    let mut content_id = [0_u8; 20];
    content_id[0] = u8::try_from(stem.len()).expect("short stem");
    let mut builder = SegmentBuilder::new("test", &content_id);
    let names: Vec<String> = (0..rows).map(|i| format!("{stem}_sym{i}")).collect();
    for (i, name) in names.iter().enumerate() {
        let row = builder.emit_row(SymbolRow {
            name,
            fql_kind: "function",
            language: "rust",
            line: u32::try_from(i + 1).expect("small fixture"),
            byte_start: 0,
            byte_end: 1,
            usages_count: 0,
        });
        for (field, values) in columns {
            builder.set_field(row, field, values[i].as_str());
        }
    }
    builder.flush(&path).expect("flush segment");
    SegmentReader::open(&path).expect("open segment")
}

/// Decode the `enrich_bitmaps` blob back into `key -> global rows`, mirroring
/// `serialize_enrich_bitmaps`.  Read byte by byte rather than through
/// `cast_slice`, because a `Vec<u8>` carries no alignment guarantee.
fn decode_enrich_blob(blob: &[u8]) -> BTreeMap<String, RoaringBitmap> {
    const ENTRY_LEN: usize = 16;
    assert_eq!(
        size_of::<EnrichEntry>(),
        ENTRY_LEN,
        "EnrichEntry changed size — this decoder mirrors serialize_enrich_bitmaps",
    );
    let u32_at =
        |at: usize| u32::from_le_bytes(blob[at..at + 4].try_into().expect("4 bytes")) as usize;
    let u16_at =
        |at: usize| u16::from_le_bytes(blob[at..at + 2].try_into().expect("2 bytes")) as usize;

    let entry_count = u32_at(0);
    let key_data_len = u32_at(4);
    let keys_at = 8 + entry_count * ENTRY_LEN;
    let bitmaps_at = keys_at + key_data_len;

    let mut decoded = BTreeMap::new();
    for i in 0..entry_count {
        let at = 8 + i * ENTRY_LEN;
        let key_start = keys_at + u32_at(at);
        let key_end = key_start + u16_at(at + 4);
        let bitmap_start = bitmaps_at + u32_at(at + 8);
        let bitmap_end = bitmap_start + u32_at(at + 12);
        let key = String::from_utf8(blob[key_start..key_end].to_vec()).expect("key is utf-8");
        let rows =
            RoaringBitmap::deserialize_from(&blob[bitmap_start..bitmap_end]).expect("bitmap");
        let _ = decoded.insert(key, rows);
    }
    decoded
}

/// Two 40-row segments carrying two enrichment columns, neither of which has a
/// posting index, so both are collected by `collect_numeric_enrichment`:
///
/// - `over` — 72 distinct values, so it passes its budget partway through the
///   SECOND segment, long after the first has contributed 40 keys.  Its last
///   eight rows then repeat values from the first segment, which is what makes
///   the difference between abandoning a pruned field and merely re-pruning it
///   observable.
/// - `under` — exactly 64 distinct values, so it never trips the budget, and
///   `u0` appears in both segments.
fn fixture_enrich_blob() -> Vec<u8> {
    assert_eq!(
        overlay_budget("over"),
        64,
        "fixture value counts are sized against the default overlay budget",
    );

    let tmp = tempfile::tempdir().expect("tempdir");

    let a_over: Vec<String> = (0..SEG_ROWS).map(|i| format!("o{i}")).collect();
    let a_under: Vec<String> = (0..SEG_ROWS).map(|i| format!("u{}", i % 32)).collect();
    // `over`'s last eight rows repeat values the FIRST segment contributed.
    // They arrive after the budget has already been passed, and because they
    // are not new values they never re-trip it — so a collector that prunes
    // and then keeps going, instead of abandoning the field, lets exactly
    // these back in as keys.
    let b_over: Vec<String> = (0..SEG_ROWS)
        .map(|i| {
            if i < 32 {
                format!("o{}", i + SEG_ROWS)
            } else {
                format!("o{}", i - 32)
            }
        })
        .collect();
    let b_under: Vec<String> = (0..SEG_ROWS)
        .map(|i| {
            if i < 32 {
                format!("u{}", i + 32)
            } else {
                format!("u{}", i - 32)
            }
        })
        .collect();

    let seg_a = segment_with_columns(tmp.path(), "a", &[("over", a_over), ("under", a_under)]);
    let seg_b = segment_with_columns(tmp.path(), "b", &[("over", b_over), ("under", b_under)]);
    let segs = vec![
        (PathBuf::from("a.rs"), "aa".to_owned(), seg_a),
        (PathBuf::from("b.rs"), "bb".to_owned(), seg_b),
    ];
    let row_offsets = [0, u32::try_from(SEG_ROWS).expect("small fixture")];
    let seg_dedup = OverlayBuilder::step45_dedup_segments(&segs);
    OverlayBuilder::step55_build_enrich_bitmaps(&segs, &row_offsets, &seg_dedup)
        .expect("build enrichment bitmaps")
}

#[test]
fn field_over_budget_drops_the_keys_an_earlier_segment_already_contributed() {
    let decoded = decode_enrich_blob(&fixture_enrich_blob());
    let survivors: Vec<&String> = decoded
        .keys()
        .filter(|key| key.starts_with("over="))
        .collect();
    assert!(
        survivors.is_empty(),
        "a field pruned in the second segment must also lose what the first \
         segment contributed, but these survived: {survivors:?}",
    );
}

#[test]
fn field_inside_budget_keeps_every_value_and_unions_across_segments() {
    let decoded = decode_enrich_blob(&fixture_enrich_blob());
    assert_eq!(
        decoded.len(),
        64,
        "the 64 values of the in-budget field, and nothing else",
    );
    assert!(decoded.keys().all(|key| key.starts_with("under=")));

    // `u0` sits at rows 0 and 32 of the first segment and at row 32 of the
    // second, which starts at global row 40.  A value spanning two segments
    // has to union its rows, not replace them.
    let rows: Vec<u32> = decoded["under=u0"].iter().collect();
    assert_eq!(rows, vec![0, 32, 72]);
}
