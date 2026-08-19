//! Unit tests for the enrichment-bitmap and name-shard steps of
//! [`OverlayBuilder`].
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
fn fixture_enrich_raw() -> HashMap<String, RoaringBitmap> {
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

/// The fixture's row sets, laid out as the `enrich_bitmaps` blob.
fn fixture_enrich_blob() -> Vec<u8> {
    let enrich_raw = fixture_enrich_raw();
    let layout = OverlayBuilder::plan_enrich_bitmaps(&enrich_raw).expect("lay out enrich blob");
    let mut blob = Vec::new();
    OverlayBuilder::write_enrich_bitmaps(&mut blob, &layout).expect("write enrichment blob");
    blob
}

/// The layout the builder produced before the blob was written straight to the
/// overlay file: each bitmap serialised into a buffer of its own, those
/// concatenated into a bitmap region, and the blob assembled from the header,
/// the entries, the keys and that region.
///
/// This is the oracle for `write_enrich_bitmaps` and the only copy of the
/// algorithm it replaced. Its saturating conversions are that algorithm's, and
/// are deliberately NOT updated to the checked ones the builder now uses: they
/// differ only for a key past 64 KiB or a region past 4 GiB, which no fixture
/// produces and which the builder now refuses outright rather than truncating.
fn reference_enrich_blob(enrich_raw: &HashMap<String, RoaringBitmap>) -> Vec<u8> {
    let mut sorted_enrich: Vec<(&String, &RoaringBitmap)> = enrich_raw.iter().collect();
    sorted_enrich.sort_by_key(|(key, _)| key.as_str());

    let mut enrich_key_bytes: Vec<u8> = Vec::new();
    let mut enrich_bitmap_data: Vec<u8> = Vec::new();
    let mut enrich_entries: Vec<EnrichEntry> = Vec::new();
    for (key, bitmap) in &sorted_enrich {
        let mut bm_bytes = Vec::new();
        bitmap
            .serialize_into(&mut bm_bytes)
            .expect("serialise bitmap");
        enrich_entries.push(EnrichEntry {
            key_offset: u32::try_from(enrich_key_bytes.len()).unwrap_or(u32::MAX),
            key_len: u16::try_from(key.len()).unwrap_or(u16::MAX),
            _pad: 0,
            bitmap_offset: u32::try_from(enrich_bitmap_data.len()).unwrap_or(u32::MAX),
            bitmap_len: u32::try_from(bm_bytes.len()).unwrap_or(u32::MAX),
        });
        enrich_key_bytes.extend_from_slice(key.as_bytes());
        enrich_bitmap_data.extend_from_slice(&bm_bytes);
    }

    let mut blob: Vec<u8> = Vec::new();
    blob.extend_from_slice(
        &u32::try_from(enrich_entries.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    blob.extend_from_slice(
        &u32::try_from(enrich_key_bytes.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    blob.extend_from_slice(cast_slice(enrich_entries.as_slice()));
    blob.extend_from_slice(&enrich_key_bytes);
    blob.extend_from_slice(&enrich_bitmap_data);
    blob
}

/// The blob is byte for byte the one the concatenating writer produced.
///
/// `write_enrich_bitmaps` takes every offset it stores from
/// `RoaringBitmap::serialized_size` and then writes the bytes separately. If
/// those two ever disagreed by so much as one byte, every entry after the
/// first would point into the wrong place — and the blob would still parse,
/// because nothing in it records where a bitmap ends except the entry that
/// already lied about where it starts.
#[test]
fn the_enrichment_blob_matches_the_concatenating_writer() {
    let enrich_raw = fixture_enrich_raw();
    assert!(
        !enrich_raw.is_empty(),
        "the fixture must produce entries, or this compares two empty blobs",
    );

    let layout = OverlayBuilder::plan_enrich_bitmaps(&enrich_raw).expect("lay out enrich blob");
    let mut streamed = Vec::new();
    OverlayBuilder::write_enrich_bitmaps(&mut streamed, &layout).expect("write blob");

    // The length the entry table was laid out against is the length the blob
    // actually writes. `blob_of_len` enforces this on every real build; here it
    // is checked against a fixture with keys and bitmaps of several sizes.
    assert_eq!(u32::try_from(streamed.len()).expect("fits u32"), layout.len,);
    assert_eq!(streamed, reference_enrich_blob(&enrich_raw));
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

/// Walk `keys` the way the sharded merge does and return what came back.
///
/// Builds an FST of `keys`, takes its [`first_byte_mask`], then for every shard
/// in [`name_shard_bounds`] either streams the shard's byte range or skips the
/// FST because the mask says the byte is absent — exactly the loop in
/// `step6_build_name_fst`. Returns the keys collected, sorted, so a caller can
/// compare against the keys it put in.
fn keys_via_shard_walk(keys: &[&[u8]]) -> Vec<Vec<u8>> {
    let mut builder = MapBuilder::memory();
    for (value, key) in keys.iter().enumerate() {
        builder.insert(key, value as u64).expect("ascending insert");
    }
    let map = FstMap::new(builder.into_inner().expect("finalise")).expect("valid fst");
    let mask = first_byte_mask(&map);

    let mut seen: Vec<Vec<u8>> = Vec::new();
    for (byte, lo, hi) in name_shard_bounds() {
        if !shard_present(&mask, byte) {
            continue;
        }
        let mut range = map.range();
        if let Some(lo) = lo {
            range = range.ge(lo);
        }
        if let Some(hi) = hi {
            range = range.lt(hi);
        }
        let mut stream = range.into_stream();
        while let Some((key, _)) = stream.next() {
            seen.push(key.to_vec());
        }
    }
    seen.sort();
    seen
}

/// Assert the shard walk over `keys` returns each of them exactly once.
fn assert_shard_walk_is_lossless(keys: &[&[u8]], what: &str) {
    let mut expected: Vec<Vec<u8>> = keys.iter().map(|key| key.to_vec()).collect();
    expected.sort();
    assert_eq!(keys_via_shard_walk(keys), expected, "{what}");
}

/// The first-byte shard partition, pinned end to end on hand-built FSTs.
///
/// `step6_build_name_fst` no longer walks each segment once; it walks each
/// segment once *per shard*, taking only the keys in that shard's byte range,
/// and skips a segment outright when [`first_byte_mask`] says the shard's byte
/// is not among the FST root's transitions. A key that falls in no shard, or in
/// a shard the mask wrongly skips, disappears from the overlay silently — the
/// merge has no way to notice, and the corpus checksums cannot see it either,
/// because no corpus is guaranteed to contain the keys that exercise the ends.
///
/// So each case below isolates ONE branch, holding a key that survives only if
/// that branch is right. Sharing an FST would defeat this: put `b""` and
/// `b"\x00a"` in one map and the transition sets bit 0 on its own, so dropping
/// the `is_final` arm would still pass.
#[test]
fn every_key_lands_in_exactly_one_first_byte_shard() {
    // The empty key is a final root, never a transition. Alone in shard 0, so
    // this fails if `first_byte_mask` stops folding `is_final` onto bit 0.
    assert_shard_walk_is_lossless(&[b"", b"abc"], "the empty key");

    // Shard 0's range is open below. This fails if it gains a `ge([0])` bound
    // — `b""` sorts before `[0]` — and the `0x00` key pins the range's top end.
    assert_shard_walk_is_lossless(&[b"", b"\x00a", b"abc"], "0x00-prefixed keys");

    // Shard 255's range is open above: there is no `[256]` to bound it with, so
    // `checked_add` yields `None`. This fails if that becomes a wrapping add,
    // which would bound the shard at `[0]` and make its range empty.
    assert_shard_walk_is_lossless(&[b"abc", b"\xff", b"\xffz"], "0xFF-prefixed keys");

    // Nothing is dropped or double-counted across the whole byte range, with a
    // multi-byte UTF-8 name reaching above the ASCII block.
    assert_shard_walk_is_lossless(
        &[
            b"",
            b"\x00a",
            b"Zed",
            b"_underscore",
            b"abc",
            b"abcd",
            b"\xc3\xa9x",
            b"\xff",
        ],
        "the whole byte range at once",
    );
}

// ── Oracles for the blobs whose index is written before their payload ────────
//
// `kind_index` and `trigram_index` are written BEFORE the `bitmap_data` region
// they both point into, and `segments` before the `segment_strings` it points
// into, so the streaming writer predicts those offsets from sizes instead of
// taking them from the bytes. Drift of one byte misplaces every entry after it
// and the file still parses, so the reader would serve wrong or empty row sets
// rather than fail. `blob_of_len` refuses that at run time; these compare the
// bytes against the algorithm that used to buffer them.

use crate::storage::columnar::overlay_writer::BLOB_ORDER;

/// Kind and trigram row sets with names of different lengths and bitmaps on
/// both sides of roaring's array/bitmap threshold, so every offset in the two
/// index blobs differs and a misplaced one shows.
fn fixture_bitmaps() -> (
    HashMap<String, RoaringBitmap>,
    HashMap<[u8; 3], RoaringBitmap>,
) {
    let small: RoaringBitmap = (0..7u32).collect();
    let medium: RoaringBitmap = (0..300u32).map(|i| i * 3).collect();
    let large: RoaringBitmap = (0..5000u32).collect();

    let mut kinds = HashMap::new();
    let _ = kinds.insert("function".to_owned(), large.clone());
    let _ = kinds.insert("if".to_owned(), small.clone());
    let _ = kinds.insert("comment_block".to_owned(), medium.clone());

    let mut trigrams = HashMap::new();
    let _ = trigrams.insert(*b"abc", medium);
    let _ = trigrams.insert(*b"zzz", small);
    let _ = trigrams.insert(*b"mno", large);
    (kinds, trigrams)
}

/// One open segment as the overlay steps pass it around: relative source path,
/// hex content ID, reader.
type FixtureSegs = Vec<(PathBuf, String, SegmentReader)>;

/// Per-segment canonical row set and the deduplicated row count that goes with
/// it, as `step45_dedup_segments` returns them.
type FixtureDedup = Vec<(RoaringBitmap, u32)>;

/// Two real flushed segments, with source paths and content IDs of different
/// lengths so the offsets in `segments` are all distinct.
fn fixture_segments() -> (FixtureSegs, FixtureDedup) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let a_values: Vec<String> = (0..SEG_ROWS).map(|i| format!("v{i}")).collect();
    let b_values: Vec<String> = (0..SEG_ROWS).map(|i| format!("w{i}")).collect();
    let seg_a = segment_with_columns(tmp.path(), "a", &[("under", a_values)]);
    let seg_b = segment_with_columns(tmp.path(), "b", &[("under", b_values)]);
    let segs = vec![
        (PathBuf::from("deep/nested/a.rs"), "aa".to_owned(), seg_a),
        (PathBuf::from("b.rs"), "bbbbbbbb".to_owned(), seg_b),
    ];
    let seg_dedup = OverlayBuilder::step45_dedup_segments(&segs);
    (segs, seg_dedup)
}

/// `kind_strings`, `kind_index`, `bitmap_data`, `trigram_index` as the builder
/// produced them before the blobs were streamed: each bitmap serialised into a
/// buffer of its own, the kind bitmaps and then the trigram bitmaps
/// concatenated into one region, and the two index blobs built from the offsets
/// that region handed back as it grew.
///
/// The oracle for `write_bitmap_blobs`, and the only copy of the algorithm it
/// replaced.
fn reference_bitmap_blobs(
    kind_postings: &HashMap<String, RoaringBitmap>,
    trigram_postings: &HashMap<[u8; 3], RoaringBitmap>,
) -> Vec<Vec<u8>> {
    let mut kind_strings: Vec<u8> = Vec::new();
    let mut kind_entries: Vec<KindEntry> = Vec::new();
    let mut bitmap_data: Vec<u8> = Vec::new();

    let mut sorted_kinds: Vec<(&String, &RoaringBitmap)> = kind_postings.iter().collect();
    sorted_kinds.sort_by_key(|(kind, _)| kind.as_str());
    for (kind_str, bitmap) in &sorted_kinds {
        let mut bm_bytes = Vec::new();
        bitmap
            .serialize_into(&mut bm_bytes)
            .expect("serialise kind bitmap");
        let kind_entry = KindEntry {
            kind_offset: u32::try_from(kind_strings.len()).expect("fits u32"),
            kind_len: u32::try_from(kind_str.len()).expect("fits u32"),
            bitmap_offset: u32::try_from(bitmap_data.len()).expect("fits u32"),
            bitmap_len: u32::try_from(bm_bytes.len()).expect("fits u32"),
        };
        kind_strings.extend_from_slice(kind_str.as_bytes());
        bitmap_data.extend_from_slice(&bm_bytes);
        kind_entries.push(kind_entry);
    }
    let kind_index: Vec<u8> = cast_slice(kind_entries.as_slice()).to_vec();

    let mut trig_entries: Vec<TrigramEntry> = Vec::new();
    let mut sorted_trigs: Vec<(&[u8; 3], &RoaringBitmap)> = trigram_postings.iter().collect();
    sorted_trigs.sort_by_key(|(trigram, _)| **trigram);
    for (trigram, bitmap) in &sorted_trigs {
        let mut bm_bytes = Vec::new();
        bitmap
            .serialize_into(&mut bm_bytes)
            .expect("serialise trigram bitmap");
        let mut tg4 = [0u8; 4];
        tg4[..3].copy_from_slice(trigram.as_ref());
        trig_entries.push(TrigramEntry {
            trigram: tg4,
            bitmap_offset: u32::try_from(bitmap_data.len()).expect("fits u32"),
            bitmap_len: u32::try_from(bm_bytes.len()).expect("fits u32"),
        });
        bitmap_data.extend_from_slice(&bm_bytes);
    }
    let trigram_index: Vec<u8> = cast_slice(trig_entries.as_slice()).to_vec();

    vec![kind_strings, kind_index, bitmap_data, trigram_index]
}

/// `segments` and `segment_strings` as the builder produced them before the
/// blobs were streamed: the string table grown record by record, each record
/// taking its offsets from the length the table had reached.
///
/// The oracle for `write_segment_blobs`.
fn reference_segment_blobs(
    segs: &[(PathBuf, String, SegmentReader)],
    seg_dedup: &[(RoaringBitmap, u32)],
) -> Vec<Vec<u8>> {
    let mut segment_strings: Vec<u8> = Vec::new();
    let mut seg_records: Vec<SegmentRecord> = Vec::new();
    for (seg_idx, (rel_path, hex, reader)) in segs.iter().enumerate() {
        let path_bytes = rel_path.to_string_lossy();
        let rec = SegmentRecord {
            row_count: reader.row_count,
            path_offset: u32::try_from(segment_strings.len()).expect("fits u32"),
            hex_id_offset: u32::try_from(segment_strings.len() + path_bytes.len())
                .expect("fits u32"),
            dedup_row_count: seg_dedup[seg_idx].1,
            path_len: u16::try_from(path_bytes.len()).expect("fits u16"),
            hex_id_len: u16::try_from(hex.len()).expect("fits u16"),
        };
        segment_strings.extend_from_slice(path_bytes.as_bytes());
        segment_strings.extend_from_slice(hex.as_bytes());
        seg_records.push(rec);
    }
    vec![cast_slice(seg_records.as_slice()).to_vec(), segment_strings]
}

/// Slice blobs `first..=last` out of `file` using the extents the writer
/// recorded for them.
fn blobs_at(file: &[u8], extents: &[(usize, usize)]) -> Vec<Vec<u8>> {
    extents
        .iter()
        .map(|&(offset, len)| file[offset..offset + len].to_vec())
        .collect()
}

/// The four kind/trigram blobs are byte for byte the ones the buffering writer
/// produced — including the shared `bitmap_data` region, where a kind bitmap
/// whose predicted length was wrong would displace every trigram entry as well.
#[test]
fn the_kind_and_trigram_blobs_match_the_buffering_writer() {
    let (kinds, trigrams) = fixture_bitmaps();

    let mut cur = io::Cursor::new(Vec::new());
    let extents: Vec<(usize, usize)> = {
        let mut w = OverlayWriter::new(&mut cur, 1).expect("start overlay");
        w.blob(BLOB_ROW_TABLE, |_| Ok(())).expect("row table");
        OverlayBuilder::write_bitmap_blobs(&mut w, kinds.clone(), trigrams.clone())
            .expect("write bitmap blobs");
        for &name in &BLOB_ORDER[5..] {
            w.blob(name, |_| Ok(())).expect("trailing blob");
        }
        let extents = (1..=4).map(|i| w.blob_extent(i)).collect();
        let _ = w.finish().expect("finish overlay");
        extents
    };

    let streamed = blobs_at(&cur.into_inner(), &extents);
    assert!(
        streamed.iter().all(|blob| !blob.is_empty()),
        "the fixture must fill all four blobs, or this compares empty vectors",
    );
    assert_eq!(streamed, reference_bitmap_blobs(&kinds, &trigrams));
}

/// The segment table and its string table are byte for byte the ones the
/// buffering writer produced.
#[test]
fn the_segment_blobs_match_the_buffering_writer() {
    let (segs, seg_dedup) = fixture_segments();

    let mut cur = io::Cursor::new(Vec::new());
    let extents: Vec<(usize, usize)> = {
        let mut w = OverlayWriter::new(&mut cur, 1).expect("start overlay");
        for &name in &BLOB_ORDER[..7] {
            w.blob(name, |_| Ok(())).expect("leading blob");
        }
        OverlayBuilder::write_segment_blobs(&mut w, &segs, &seg_dedup)
            .expect("write segment blobs");
        for &name in &BLOB_ORDER[9..] {
            w.blob(name, |_| Ok(())).expect("trailing blob");
        }
        let extents = (7..=8).map(|i| w.blob_extent(i)).collect();
        let _ = w.finish().expect("finish overlay");
        extents
    };

    let streamed = blobs_at(&cur.into_inner(), &extents);
    assert!(
        streamed.iter().all(|blob| !blob.is_empty()),
        "the fixture must fill both blobs, or this compares empty vectors",
    );
    assert_eq!(streamed, reference_segment_blobs(&segs, &seg_dedup));
}
