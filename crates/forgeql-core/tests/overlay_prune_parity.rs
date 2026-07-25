//! Overlay/columnar parity: segment pruning and fast paths.
//!
//! Zone-map range pruning, path-glob and short-prefix segment prefilters,
//! enrichment posting filters, and the enrichment-only fast path must each
//! return the same rows as the legacy backend — including empty results when a
//! predicate prunes every segment.

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

/// `IN 'nonexistent/**'` should return zero rows because the segment path
/// prefilter drops all segments whose source_path does not match the glob,
/// so `materialize_all` is never entered for any segment.
///
/// This exercises `segments_passing_path_filter` directly.
#[test]
fn path_glob_prunes_all_segments() {
    use forgeql_core::ir::Clauses;
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = Clauses {
        in_glob: Some("nonexistent/**".to_owned()),
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find_symbols with non-matching glob");

    assert_eq!(
        results.len(),
        0,
        "expected 0 rows when IN glob matches no segments, got {}",
        results.len()
    );
}

/// Verify that `WHERE has_doc = 'true'` and `WHERE has_doc = 'false'` both
/// return byte-equivalent results to the legacy backend after the enrichment
/// posting prefilter is applied.
///
/// This exercises `prefilter_enrichment_postings` for both values of a
/// boolean enrichment field.
#[test]
fn enrichment_posting_filter_parity() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    for value in &["true", "false"] {
        let clauses = forgeql_core::ir::Clauses {
            where_predicates: vec![Predicate {
                field: "has_doc".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String((*value).to_owned()),
            }],
            ..forgeql_core::ir::Clauses::default()
        };

        let columnar = storage
            .find_symbols(&clauses, std::path::Path::new("."))
            .expect("columnar find");

        // Compute the legacy count by scanning the symbol table directly.
        let legacy_count = table
            .rows
            .iter()
            .filter(|r| {
                table
                    .resolve_fields(&r.fields)
                    .iter()
                    .any(|(k, v)| k == "has_doc" && v.as_str() == *value)
            })
            .count();

        assert_eq!(
            columnar.len(),
            legacy_count,
            "has_doc='{value}' count mismatch: columnar={} legacy={legacy_count}",
            columnar.len()
        );

        // Every returned row must actually have the correct has_doc value.
        for r in &columnar {
            let has_doc = r.fields.get("has_doc").map(String::as_str);
            assert_eq!(
                has_doc,
                Some(*value),
                "row '{}' has wrong has_doc: expected '{value}', got {:?}",
                r.name,
                has_doc
            );
        }
    }
}

/// Verify that combining `WHERE has_doc = 'true'` with `IN 'canonical.cpp'`
/// on a 2-segment overlay (cpp + rs) returns only the cpp rows that have
/// has_doc=true, parity-equal to the legacy backend.
///
/// This exercises both prefilters together: `segments_passing_path_filter`
/// prunes the rs segment, and `prefilter_enrichment_postings` prunes rows
/// inside the cpp segment.
#[test]
fn combined_path_glob_and_enrichment_parity() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;
    use forgeql_core::storage::columnar::overlay::Overlay;
    use forgeql_core::storage::columnar::{ColumnarStorage, OverlayBuilder, SegmentReader};

    let table_cpp = index_fixture(&CppLanguage, "canonical.cpp");
    let table_rust = index_fixture(&RustLanguage, "canonical.rs");

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");

    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let rs_cid = build_segment(&table_rust, &rs_path, &segments_dir);

    let mut segment_map: HashMap<std::path::PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_cid);
    let _ = segment_map.insert(rs_path, rs_cid);

    let overlay_path = overlays_dir.join("test").join("combined_test.bin");
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

    // Query: WHERE has_doc='true' IN 'canonical.cpp'
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "has_doc".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("true".to_owned()),
        }],
        in_glob: Some("canonical.cpp".to_owned()),
        ..Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");

    // Legacy: only cpp table rows with has_doc='true'
    let legacy_count = table_cpp
        .rows
        .iter()
        .filter(|r| {
            table_cpp
                .resolve_fields(&r.fields)
                .iter()
                .any(|(k, v)| k == "has_doc" && v == "true")
        })
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "combined glob+enrichment: columnar={} legacy={legacy_count}",
        columnar.len()
    );

    // Every returned row must have has_doc='true'.
    for r in &columnar {
        assert_eq!(
            r.fields.get("has_doc").map(String::as_str),
            Some("true"),
            "row '{}' missing has_doc='true'",
            r.name
        );
        // No row from canonical.rs should appear.
        if let Some(ref path) = r.path {
            assert!(
                path.to_string_lossy().contains("canonical.cpp"),
                "row '{}' came from non-cpp path: {}",
                r.name,
                path.display()
            );
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Phase 06d parity tests
// ─────────────────────────────────────────────────────────────────────────────

/// Task 1 — `WHERE fql_kind = 'nonexistent'` must return zero rows, not a
/// full scan. Exercises the fql_kind miss -> Some(empty) fix.
#[test]
fn unknown_fql_kind_returns_empty_no_segment_open() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("___no_such_kind___".to_owned()),
        }],
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find_symbols with unknown fql_kind");

    assert_eq!(
        results.len(),
        0,
        "expected 0 rows for unknown fql_kind, got {}",
        results.len()
    );
}

/// Task 2 — `WHERE line > <max_line>` returns zero rows via zone-map prune.
#[test]
fn range_predicate_prunes_segments_via_zone_map() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    // Use a line number that is guaranteed to exceed any real source file.
    // The segment's zone map (max_line <= a few thousand) must prune it,
    // so the result must be empty.
    let beyond_any_line: i64 = i64::from(u32::MAX);

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "line".to_owned(),
            op: CompareOp::Gt,
            value: PredicateValue::Number(beyond_any_line),
        }],
        ..Clauses::default()
    };

    let results = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("find_symbols with out-of-range line");

    assert_eq!(
        results.len(),
        0,
        "expected 0 rows when line > {beyond_any_line}, got {}",
        results.len()
    );
}

/// Task 3 — `WHERE name LIKE 'f%'` via short-prefix index matches legacy count.
#[test]
fn short_prefix_like_uses_index() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    let prefix = "f";
    let pattern = format!("{prefix}%");

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "name".to_owned(),
            op: CompareOp::Like,
            value: PredicateValue::String(pattern.clone()),
        }],
        ..Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar find");

    let legacy_count = table
        .rows
        .iter()
        .filter(|r| table.name_of(r).to_ascii_lowercase().starts_with(prefix))
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "short-prefix LIKE '{pattern}': columnar={} legacy={legacy_count}",
        columnar.len()
    );
}

/// Task 3 combined — short-prefix + path-glob + range must match legacy count.
#[test]
fn combined_short_prefix_and_path_glob_and_range_matches_legacy() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    let clauses = Clauses {
        where_predicates: vec![
            Predicate {
                field: "name".to_owned(),
                op: CompareOp::Like,
                value: PredicateValue::String("f%".to_owned()),
            },
            Predicate {
                field: "line".to_owned(),
                op: CompareOp::Gte,
                value: PredicateValue::Number(1),
            },
        ],
        in_glob: Some("canonical.cpp".to_owned()),
        ..Clauses::default()
    };

    let columnar = storage
        .find_symbols(&clauses, std::path::Path::new("."))
        .expect("columnar combined find");

    // line >= 1 is trivially true for every real symbol; count by name only.
    let legacy_count = table
        .rows
        .iter()
        .map(|r| table.name_of(r).to_owned())
        .filter(|n| n.to_ascii_lowercase().starts_with('f'))
        .count();

    assert_eq!(
        columnar.len(),
        legacy_count,
        "combined: columnar={} legacy={legacy_count}",
        columnar.len()
    );
}

/// `WHERE enrichment_field = X IN glob` with no fql_kind/name predicate
/// triggers the fast-path in find_symbols (skip global bitmap → iterate only
/// path-filtered segments directly).  Result must be identical to the normal
/// path and match the legacy backend count.
#[test]
fn enrichment_only_fast_path_parity() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (table, _tmp, storage) = single_segment_cpp_overlay();

    for value in &["true", "false"] {
        // has_doc only, plus IN glob → triggers fast-path (no indexed predicate)
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "has_doc".to_owned(),
                op: CompareOp::Eq,
                value: PredicateValue::String((*value).to_owned()),
            }],
            in_glob: Some("canonical.cpp".to_owned()),
            ..Clauses::default()
        };

        let columnar = storage
            .find_symbols(&clauses, std::path::Path::new("."))
            .expect("fast-path find");

        // Legacy count: rows with matching has_doc field.
        let legacy_count = table
            .rows
            .iter()
            .filter(|r| {
                table
                    .resolve_fields(&r.fields)
                    .iter()
                    .any(|(k, v): (&String, &String)| k == "has_doc" && v.as_str() == *value)
            })
            .count();

        assert_eq!(
            columnar.len(),
            legacy_count,
            "fast-path has_doc='{value}': columnar={} legacy={legacy_count}",
            columnar.len()
        );

        for r in &columnar {
            let has_doc = r.fields.get("has_doc").map(String::as_str);
            assert_eq!(
                has_doc,
                Some(*value),
                "fast-path row '{}' has wrong has_doc: expected '{value}', got {:?}",
                r.name,
                has_doc
            );
        }
    }
}

/// `WHERE line < 0` must return empty immediately — no u32 line value can
/// be negative.  The negative-value short-circuit in the zone-map wiring
/// clears all candidates without opening any segment or reading zone-map files.
#[test]
fn negative_line_predicate_returns_empty() {
    use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
    use forgeql_core::storage::StorageEngine;

    let (_table, _tmp, storage) = single_segment_cpp_overlay();

    for &(ref op, val) in &[
        (CompareOp::Lt, -1_i64),
        (CompareOp::Lte, -1_i64),
        (CompareOp::Eq, -1_i64),
        (CompareOp::Lt, 0_i64), // WHERE line < 0 — val=0, still impossible for u32
    ] {
        let clauses = Clauses {
            where_predicates: vec![Predicate {
                field: "line".to_owned(),
                op: *op,
                value: PredicateValue::Number(val),
            }],
            ..Clauses::default()
        };

        let result = storage
            .find_symbols(&clauses, std::path::Path::new("."))
            .expect("find should not error");

        assert!(
            result.is_empty(),
            "WHERE line {op:?} {val} should return empty, got {} rows",
            result.len()
        );
    }
}
