//! Unit tests for [`Overlay`](super::Overlay).

use std::collections::HashMap;
use std::io::{BufWriter, Write};

use roaring::RoaringBitmap;

use super::*;
use crate::storage::columnar::overlay_writer::{self, write_v3};

/// Build a minimal FQOV v3 overlay in a tempfile.
///
/// Only `trigram_postings` and `row_count` are populated; all other blobs
/// are empty or trivially valid.
fn make_test_overlay(
    trigram_postings: &HashMap<[u8; 3], Vec<u8>>,
    row_count: u32,
) -> tempfile::NamedTempFile {
    let empty_fst = fst::MapBuilder::memory()
        .into_inner()
        .expect("empty FST bytes");
    let row_table: Vec<RowPtr> = (0..row_count)
        .map(|i| RowPtr {
            segment_idx: 0,
            local_row_idx: i,
        })
        .collect();

    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    {
        let mut f = BufWriter::new(tmp.as_file());
        write_v3(
            &mut f,
            &overlay_writer::WriteV3Params {
                generation: 1,
                global_row_table: &row_table,
                kind_postings: &HashMap::new(),
                trigram_postings,
                name_fst_bytes: &empty_fst,
                name_postings_bytes: &[],
                segment_metas: &[],
                index_files_bytes: &[],
                enrich_bitmaps_bytes: &[],
                file_entries_bytes: &[],
                usages_count_fst_bytes: &[],
            },
        )
        .expect("write_v3");
        f.flush().expect("flush");
    }
    tmp
}

/// Attempting to open a non-existent file returns an error (not a panic).
#[test]
fn open_missing_file_returns_err() {
    let result = Overlay::open(Path::new("/nonexistent/overlay.bin"));
    assert!(result.is_err(), "expected Err for missing file");
}

/// A file with invalid magic returns a descriptive error.
#[test]
fn open_wrong_magic_returns_err() {
    let tmp = tempfile::NamedTempFile::new().expect("tempfile");
    let mut data = vec![0u8; HEADER_LEN];
    data[..4].copy_from_slice(b"XXXX");
    std::fs::write(tmp.path(), &data).expect("write");
    match Overlay::open(tmp.path()) {
        Ok(_) => panic!("expected Err for wrong magic, but got Ok"),
        Err(e) => {
            let msg = format!("{e:#}");
            assert!(msg.contains("magic"), "error should mention magic: {msg}");
        }
    }
}

/// `name_substring_candidates` returns `None` for sub-trigram inputs.
#[test]
fn substring_candidates_none_for_short_input() {
    // Non-empty trigram index so we reach the length check.
    let mut trig = HashMap::new();
    let bm: RoaringBitmap = std::iter::once(0u32).collect();
    let mut bm_bytes = Vec::new();
    bm.serialize_into(&mut bm_bytes).unwrap();
    trig.insert(*b"abc", bm_bytes);

    let tmp = make_test_overlay(&trig, 1);
    let overlay = Overlay::open(tmp.path()).expect("open");

    assert!(overlay.name_substring_candidates("ab").is_none());
    assert!(overlay.name_substring_candidates("").is_none());
}

/// `name_substring_candidates` intersects per-trigram bitmaps correctly
/// and short-circuits to `Some(empty)` when a trigram is absent.
#[test]
fn substring_candidates_intersects_and_misses() {
    let rows: RoaringBitmap = [0u32, 2].iter().copied().collect();
    let mut trig = HashMap::new();
    for t in [*b"alp", *b"lph", *b"pha"] {
        let mut bytes = Vec::new();
        rows.serialize_into(&mut bytes).unwrap();
        trig.insert(t, bytes);
    }

    let tmp = make_test_overlay(&trig, 3);
    let overlay = Overlay::open(tmp.path()).expect("open");

    // Single trigram "alp" → {0, 2}.
    let got = overlay.name_substring_candidates("alp").expect("some");
    assert_eq!(got.iter().collect::<Vec<_>>(), vec![0u32, 2]);

    // "alpha" trigrams: alp, lph, pha — all present, intersection {0, 2}.
    let got = overlay.name_substring_candidates("alpha").expect("some");
    assert_eq!(got.iter().collect::<Vec<_>>(), vec![0u32, 2]);

    // ASCII case-insensitivity.
    let got = overlay.name_substring_candidates("ALP").expect("some");
    assert_eq!(got.iter().collect::<Vec<_>>(), vec![0u32, 2]);

    // Missing trigram → Some(empty).
    let got = overlay.name_substring_candidates("zzz").expect("some");
    assert!(got.is_empty());
}
