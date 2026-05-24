//! Criterion micro-benchmarks for Phase 06d: zone-map prune and short-prefix
//! name-index optimisations in the columnar storage backend.
//!
//! Four benchmark functions are provided:
//!
//! - `range_high_selectivity`   — `WHERE line > <beyond_max>` (zone-map prunes all segments)
//! - `range_medium_selectivity` — `WHERE line > 1` (zone-map passes all segments; residual filter)
//! - `short_prefix_like`        — `WHERE name LIKE 'f%'` (short-prefix bitmap index)
//! - `combined`                 — `WHERE name LIKE 'f%' AND line > 1 IN 'canonical.cpp'`
//!
//! Run with:
//!   cargo bench -p forgeql-core --bench columnar_range --features test-helpers

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::doc_markdown,
    clippy::missing_docs_in_private_items,
    clippy::missing_panics_doc,
    clippy::semicolon_if_nothing_returned,
    missing_docs,
    unused_results
)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{SymbolTable, index_file};
use forgeql_core::ast::lang::{
    CppLanguageInline, LanguageRegistry, LanguageSupport, RustLanguageInline,
};
use forgeql_core::ir::{Clauses, CompareOp, Predicate, PredicateValue};
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{
    ColumnarStorage, OverlayBuilder, SegmentBuilder, SegmentReader, SymbolRow,
};
use tempfile::TempDir;

// ── fixture helpers ───────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

fn fixture_path(name: &str) -> PathBuf {
    fixtures_dir().join(name)
}

fn index_fixture(lang: &dyn LanguageSupport, filename: &str) -> SymbolTable {
    let path = fixture_path(filename);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    index_file(&mut parser, &path, &mut table, &enrichers, lang, None, None).expect("index_file");
    table
}

fn build_segment(
    table: &SymbolTable,
    abs_path: &std::path::Path,
    segments_dir: &std::path::Path,
) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    abs_path.hash(&mut h);
    let content_id: Vec<u8> = h.finish().to_le_bytes().to_vec();
    let hex = content_id.iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    });
    let seg_dir = segments_dir.join("bench-range").join(&hex);
    let mut builder = SegmentBuilder::new("bench-range", &content_id);
    for row in &table.rows {
        let row_id = builder.emit_row(SymbolRow {
            name: table.name_of(row),
            fql_kind: table.fql_kind_of(row),
            language: table.language_of(row),
            line: u32::try_from(row.line).unwrap_or(u32::MAX),
            byte_start: u32::try_from(row.byte_range.start).unwrap_or(u32::MAX),
            byte_end: u32::try_from(row.byte_range.end).unwrap_or(u32::MAX),
            usages_count: row.usages_count,
        });
        for (k, v) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &k, v.as_str());
        }
    }
    builder.flush(&seg_dir).expect("segment flush");
    content_id
}

/// Build a two-segment (canonical.cpp + canonical.rs) overlay.
fn build_two_segment_overlay() -> (TempDir, ColumnarStorage) {
    let cpp_lang = CppLanguageInline;
    let rs_lang = RustLanguageInline;
    let table_cpp = index_fixture(&cpp_lang, "canonical.cpp");
    let table_rs = index_fixture(&rs_lang, "canonical.rs");

    let tmp = TempDir::new().expect("tempdir");
    let segments_dir = tmp.path().join("segments");
    let overlays_dir = tmp.path().join("overlays");

    let cpp_path = fixture_path("canonical.cpp");
    let rs_path = fixture_path("canonical.rs");

    let cpp_cid = build_segment(&table_cpp, &cpp_path, &segments_dir);
    let rs_cid = build_segment(&table_rs, &rs_path, &segments_dir);

    let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let _ = segment_map.insert(cpp_path, cpp_cid);
    let _ = segment_map.insert(rs_path, rs_cid);

    let overlay_path = overlays_dir.join("bench-range").join("overlay.bin");
    OverlayBuilder::new(
        "bench-range",
        segments_dir.clone(),
        fixtures_dir(),
        segment_map,
    )
    .build_and_persist(&overlay_path)
    .expect("overlay build");

    let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
    let segments: Vec<Arc<SegmentReader>> = overlay
        .segments()
        .iter()
        .map(|m| {
            Arc::new(
                SegmentReader::open(&segments_dir.join("bench-range").join(&m.hex_content_id))
                    .expect("SegmentReader::open"),
            )
        })
        .collect();

    let registry = Arc::new(LanguageRegistry::new(vec![]));
    let storage = ColumnarStorage::new(fixtures_dir(), segments, overlay, registry);
    (tmp, storage)
}

// ── benchmarks ────────────────────────────────────────────────────────────────

/// `WHERE line > u32::MAX` — zone-map eliminates both segments immediately.
/// This measures the overhead of the zone-map lookup path with zero surviving
/// segments (best-case for the prune).
fn bench_range_high_selectivity(c: &mut Criterion) {
    let (_tmp, storage) = build_two_segment_overlay();
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "line".to_owned(),
            op: CompareOp::Gt,
            value: PredicateValue::Number(i64::from(u32::MAX)),
        }],
        ..Clauses::default()
    };
    c.bench_function("range_high_selectivity", |b| {
        b.iter(|| {
            storage
                .find_symbols(black_box(&clauses), std::path::Path::new("."))
                .expect("find_symbols")
        })
    });
}

/// `WHERE line > 1` — zone-map passes both segments (all real lines > 1 not
/// possible to prune); residual materialization filter handles it.
/// This measures the cost when zone-map adds no benefit.
fn bench_range_medium_selectivity(c: &mut Criterion) {
    let (_tmp, storage) = build_two_segment_overlay();
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "line".to_owned(),
            op: CompareOp::Gt,
            value: PredicateValue::Number(1),
        }],
        ..Clauses::default()
    };
    c.bench_function("range_medium_selectivity", |b| {
        b.iter(|| {
            storage
                .find_symbols(black_box(&clauses), std::path::Path::new("."))
                .expect("find_symbols")
        })
    });
}

/// `WHERE name LIKE 'f%'` — exercises the short-prefix bitmap index (1-char).
/// Both segments are candidates; the prefix bitmap narrows the global row set
/// before any segment is opened.
fn bench_short_prefix_like(c: &mut Criterion) {
    let (_tmp, storage) = build_two_segment_overlay();
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "name".to_owned(),
            op: CompareOp::Like,
            value: PredicateValue::String("f%".to_owned()),
        }],
        ..Clauses::default()
    };
    c.bench_function("short_prefix_like", |b| {
        b.iter(|| {
            storage
                .find_symbols(black_box(&clauses), std::path::Path::new("."))
                .expect("find_symbols")
        })
    });
}

/// `WHERE name LIKE 'f%' AND line > 1 IN 'canonical.cpp'` — combines the
/// short-prefix index, zone-map prune, and path prefilter in one query.
fn bench_combined(c: &mut Criterion) {
    let (_tmp, storage) = build_two_segment_overlay();
    let clauses = Clauses {
        where_predicates: vec![
            Predicate {
                field: "name".to_owned(),
                op: CompareOp::Like,
                value: PredicateValue::String("f%".to_owned()),
            },
            Predicate {
                field: "line".to_owned(),
                op: CompareOp::Gt,
                value: PredicateValue::Number(1),
            },
        ],
        in_glob: Some("canonical.cpp".to_owned()),
        ..Clauses::default()
    };
    c.bench_function("combined", |b| {
        b.iter(|| {
            storage
                .find_symbols(black_box(&clauses), std::path::Path::new("."))
                .expect("find_symbols")
        })
    });
}

criterion_group!(
    benches,
    bench_range_high_selectivity,
    bench_range_medium_selectivity,
    bench_short_prefix_like,
    bench_combined
);
criterion_main!(benches);
