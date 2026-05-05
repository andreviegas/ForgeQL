//! Phase 05 — `parity_find` gate test.
//!
//! Runs ≥200 corpus queries against both:
//!
//! - **Legacy baseline**: `apply_clauses` applied to all `SymbolTable` rows
//!   materialised as `SymbolMatch`.  Functionally equivalent to
//!   `LegacyMemoryStorage::find_symbols` for small datasets where the
//!   prefilter optimisation does not affect the result set.
//!
//! - **Columnar backend**: `ColumnarStorage::find_symbols` on a 2-segment
//!   overlay built from `canonical.cpp` + `canonical.rs`.
//!
//! Results are canonicalised by sorting on `(name, fql_kind, line)` before
//! comparison, so ORDER BY differences do not cause false failures.
//! GROUP BY queries are excluded (Issue 7: documented accepted deviation).
//!
//! Gate command:
//! ```
//! cargo test --package forgeql-core --test parity_find
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::cast_possible_truncation
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{IndexRow, SymbolTable, index_file};
use forgeql_core::ast::lang::{CppLanguageInline, LanguageSupport, RustLanguageInline};
use forgeql_core::filter::apply_clauses;
use forgeql_core::ir::{Clauses, CompareOp, OrderBy, Predicate, PredicateValue, SortDirection};
use forgeql_core::result::SymbolMatch;
use forgeql_core::storage::StorageEngine;
use forgeql_core::storage::columnar::overlay::Overlay;
use forgeql_core::storage::columnar::{
    ColumnarStorage, OverlayBuilder, SegmentBuilder, SegmentReader,
};
use tempfile::TempDir;

// ── fixtures ─────────────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

fn fixture_path(filename: &str) -> PathBuf {
    let p = fixtures_dir().join(filename);
    assert!(p.exists(), "fixture missing: {}", p.display());
    p
}

fn index_fixture(lang: &dyn LanguageSupport, filename: &str) -> SymbolTable {
    let path = fixture_path(filename);
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");
    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();
    let count = index_file(&mut parser, &path, &mut table, &enrichers, lang, None, None)
        .expect("index_file should succeed");
    assert!(count > 0, "expected at least one row in {filename}");
    table
}

fn build_segment(table: &SymbolTable, abs_source_path: &Path, segments_dir: &Path) -> Vec<u8> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut h = DefaultHasher::new();
    abs_source_path.hash(&mut h);
    let hash_u64 = h.finish();
    let content_id: Vec<u8> = hash_u64.to_le_bytes().to_vec();

    let hex = content_id.iter().fold(String::new(), |mut acc, b| {
        use std::fmt::Write as _;
        let _ = write!(acc, "{b:02x}");
        acc
    });

    let seg_dir = segments_dir.join("test").join(&hex);
    let mut builder = SegmentBuilder::new("test", &content_id);
    for row in &table.rows {
        let row_id = builder.emit_row(
            table.name_of(row),
            table.fql_kind_of(row),
            table.language_of(row),
            row.line as u32,
            row.byte_range.start as u32,
            row.byte_range.end as u32,
            row.usages_count,
        );
        for (key, val) in table.resolve_fields(&row.fields) {
            builder.set_field(row_id, &key, val.as_str());
        }
    }
    builder.flush(&seg_dir).expect("segment flush");
    content_id
}

// ── dual backend ─────────────────────────────────────────────────────────────

struct DualBackend {
    cpp_table: SymbolTable,
    rust_table: SymbolTable,
    columnar: ColumnarStorage,
    // keeps TempDir alive for the lifetime of the backend
    _tmp: TempDir,
}

impl DualBackend {
    fn build() -> Self {
        let cpp_table = index_fixture(&CppLanguageInline, "canonical.cpp");
        let rust_table = index_fixture(&RustLanguageInline, "canonical.rs");

        let tmp = TempDir::new().expect("tempdir");
        let segments_dir = tmp.path().join("segments");
        let overlays_dir = tmp.path().join("overlays");

        let cpp_path = fixture_path("canonical.cpp");
        let rs_path = fixture_path("canonical.rs");

        let cpp_cid = build_segment(&cpp_table, &cpp_path, &segments_dir);
        let rs_cid = build_segment(&rust_table, &rs_path, &segments_dir);

        let mut segment_map: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        let _ = segment_map.insert(cpp_path, cpp_cid);
        let _ = segment_map.insert(rs_path, rs_cid);

        let overlay_path = overlays_dir.join("test").join("parity_find.bin");
        OverlayBuilder::new("test", segments_dir.clone(), fixtures_dir(), segment_map)
            .build_and_persist(&overlay_path)
            .expect("overlay build");

        let overlay = Overlay::open(&overlay_path).expect("Overlay::open");
        let segs: Vec<Arc<SegmentReader>> = overlay
            .segments()
            .iter()
            .map(|meta| {
                let seg_dir = segments_dir.join("test").join(&meta.hex_content_id);
                Arc::new(SegmentReader::open(&seg_dir).expect("SegmentReader::open"))
            })
            .collect();
        let columnar = ColumnarStorage::new(fixtures_dir(), segs, overlay);

        Self {
            cpp_table,
            rust_table,
            columnar,
            _tmp: tmp,
        }
    }

    /// Run `clauses` against the legacy baseline.
    ///
    /// Materialises all rows from both `SymbolTable`s into `SymbolMatch` then
    /// delegates to `apply_clauses`.  Functionally equivalent to
    /// `LegacyMemoryStorage::find_symbols` for our small fixture set.
    fn legacy_run(&self, clauses: &Clauses) -> Vec<(String, String, usize)> {
        let mut results: Vec<SymbolMatch> = Vec::new();
        for table in [&self.cpp_table, &self.rust_table] {
            for row in &table.rows {
                results.push(row_to_match(row, table));
            }
        }
        apply_clauses(&mut results, clauses);
        to_key_tuples(&results)
    }

    /// Run `clauses` against the columnar backend.
    fn columnar_run(&self, clauses: &Clauses) -> Vec<(String, String, usize)> {
        let results = self
            .columnar
            .find_symbols(clauses, &fixtures_dir())
            .expect("columnar find_symbols");
        to_key_tuples(&results)
    }
}

fn row_to_match(row: &IndexRow, table: &SymbolTable) -> SymbolMatch {
    // Mirror the RowRef ClauseTarget impl: return None for empty strings
    // so that NotEq predicates behave identically to the real legacy backend.
    let fql_kind = table.fql_kind_of(row);
    let language = table.language_of(row);
    let node_kind = table.node_kind_of(row);
    SymbolMatch {
        name: table.name_of(row).to_owned(),
        node_kind: if node_kind.is_empty() {
            None
        } else {
            Some(node_kind.to_owned())
        },
        fql_kind: if fql_kind.is_empty() {
            None
        } else {
            Some(fql_kind.to_owned())
        },
        language: if language.is_empty() {
            None
        } else {
            Some(language.to_owned())
        },
        path: Some(table.path_of(row).to_path_buf()),
        line: Some(row.line),
        usages_count: Some(row.usages_count as usize),
        fields: table.resolve_fields(&row.fields),
        count: None,
    }
}

/// Canonicalise a result set to `(name, fql_kind, line)` sorted tuples.
///
/// Sorting before comparison means ORDER BY differences do not cause false
/// failures — we verify SET equality, not order.
fn to_key_tuples(results: &[SymbolMatch]) -> Vec<(String, String, usize)> {
    let mut v: Vec<_> = results
        .iter()
        .map(|r| {
            (
                r.name.clone(),
                r.fql_kind.clone().unwrap_or_default(),
                r.line.unwrap_or(0),
            )
        })
        .collect();
    v.sort_unstable();
    v
}

// ── clause builder helpers ────────────────────────────────────────────────────

fn all() -> Clauses {
    Clauses::default()
}

fn pred(field: &str, op: CompareOp, val: &str) -> Predicate {
    Predicate {
        field: field.to_owned(),
        op,
        value: PredicateValue::String(val.to_owned()),
    }
}

fn num_pred(field: &str, op: CompareOp, val: i64) -> Predicate {
    Predicate {
        field: field.to_owned(),
        op,
        value: PredicateValue::Number(val),
    }
}

fn with_preds(predicates: Vec<Predicate>) -> Clauses {
    Clauses {
        where_predicates: predicates,
        ..Clauses::default()
    }
}

fn kind_eq(k: &str) -> Clauses {
    with_preds(vec![pred("fql_kind", CompareOp::Eq, k)])
}

fn kind_ne(k: &str) -> Clauses {
    with_preds(vec![pred("fql_kind", CompareOp::NotEq, k)])
}

fn name_eq(n: &str) -> Clauses {
    with_preds(vec![pred("name", CompareOp::Eq, n)])
}

fn name_ne(n: &str) -> Clauses {
    with_preds(vec![pred("name", CompareOp::NotEq, n)])
}

fn name_like(p: &str) -> Clauses {
    with_preds(vec![pred("name", CompareOp::Like, p)])
}

fn name_not_like(p: &str) -> Clauses {
    with_preds(vec![pred("name", CompareOp::NotLike, p)])
}

fn field_eq(f: &str, v: &str) -> Clauses {
    with_preds(vec![pred(f, CompareOp::Eq, v)])
}

fn field_ne(f: &str, v: &str) -> Clauses {
    with_preds(vec![pred(f, CompareOp::NotEq, v)])
}

fn line_gt(n: i64) -> Clauses {
    with_preds(vec![num_pred("line", CompareOp::Gt, n)])
}

fn line_ge(n: i64) -> Clauses {
    with_preds(vec![num_pred("line", CompareOp::Gte, n)])
}

fn line_lt(n: i64) -> Clauses {
    with_preds(vec![num_pred("line", CompareOp::Lt, n)])
}

fn line_le(n: i64) -> Clauses {
    with_preds(vec![num_pred("line", CompareOp::Lte, n)])
}

fn order_asc(field: &str) -> Clauses {
    Clauses {
        order_by: Some(OrderBy {
            field: field.to_owned(),
            direction: SortDirection::Asc,
        }),
        ..Clauses::default()
    }
}

fn order_desc(field: &str) -> Clauses {
    Clauses {
        order_by: Some(OrderBy {
            field: field.to_owned(),
            direction: SortDirection::Desc,
        }),
        ..Clauses::default()
    }
}

fn add_order(mut c: Clauses, field: &str, dir: SortDirection) -> Clauses {
    c.order_by = Some(OrderBy {
        field: field.to_owned(),
        direction: dir,
    });
    c
}

const fn add_limit(mut c: Clauses, n: usize) -> Clauses {
    c.limit = Some(n);
    c
}

#[allow(clippy::missing_const_for_fn)]
fn add_offset(mut c: Clauses, n: usize) -> Clauses {
    c.offset = Some(n);
    c
}

fn add_pred_str(mut c: Clauses, field: &str, op: CompareOp, val: &str) -> Clauses {
    c.where_predicates.push(pred(field, op, val));
    c
}

#[allow(dead_code)]
fn add_pred_num(mut c: Clauses, field: &str, op: CompareOp, val: i64) -> Clauses {
    c.where_predicates.push(num_pred(field, op, val));
    c
}

// ── corpus ────────────────────────────────────────────────────────────────────

/// Build the ≥200-query parity corpus.
///
/// Each entry is `(label, clauses)`.  GROUP BY queries are excluded
/// (see Phase05-issues.md §7 — accepted deviation).
fn corpus() -> Vec<(String, Clauses)> {
    let mut v: Vec<(String, Clauses)> = Vec::new();

    // ── Group 1: Unconstrained (1) ─────────────────────────────────────────
    v.push(("g01_all".into(), all()));

    // ── Group 2: Exact fql_kind = X (8) ────────────────────────────────────
    for k in [
        "function",
        "struct",
        "enum",
        "variable",
        "constant",
        "field",
        "enum_variant",
        "nonexistent_kind_xyz",
    ] {
        v.push((format!("g02_kind_eq_{k}"), kind_eq(k)));
    }

    // ── Group 3: fql_kind != X (5) ─────────────────────────────────────────
    for k in ["function", "struct", "enum", "variable", "constant"] {
        v.push((format!("g03_kind_ne_{k}"), kind_ne(k)));
    }

    // ── Group 4: Exact name match (24) ──────────────────────────────────────
    for n in [
        "foo",
        "bar",
        "factorial",
        "process",
        "helper",
        "transform",
        "checker",
        "shadowed",
        "escaping",
        "switcher",
        "distant",
        "caller",
        "noop",
        "no_default",
        "deeply_nested",
        "Motor",
        "State",
        "speed",
        "count",
        "hex_value",
        "bin_value",
        "pi",
        "MAGIC",
        "Idle",
    ] {
        v.push((format!("g04_name_eq_{n}"), name_eq(n)));
    }

    // ── Group 5: Name != X (6) ──────────────────────────────────────────────
    for n in ["foo", "bar", "Motor", "State", "MAGIC", "count"] {
        v.push((format!("g05_name_ne_{n}"), name_ne(n)));
    }

    // ── Group 6: LIKE prefix (14) ────────────────────────────────────────────
    for p in [
        "f%", "b%", "p%", "h%", "c%", "s%", "e%", "d%", "n%", "t%", "M%", "S%", "I%", "R%",
    ] {
        v.push((format!("g06_name_like_{p}"), name_like(p)));
    }

    // ── Group 7: LIKE suffix (7) ─────────────────────────────────────────────
    for p in ["%er", "%or", "%ed", "%al", "%t", "%e", "%d"] {
        v.push((format!("g07_name_like_{p}"), name_like(p)));
    }

    // ── Group 8: LIKE contains (7) ───────────────────────────────────────────
    for p in ["%oo%", "%ar%", "%at%", "%or%", "%ee%", "%al%", "%er%"] {
        v.push((format!("g08_name_like_{p}"), name_like(p)));
    }

    // ── Group 9: NOT LIKE (8) ────────────────────────────────────────────────
    for p in [
        "f%",
        "b%",
        "M%",
        "S%",
        "%er",
        "%ed",
        "%oo%",
        "nonexistent_prefix_xyz%",
    ] {
        v.push((format!("g09_name_not_like_{p}"), name_not_like(p)));
    }

    // ── Group 10: Enrichment field = value (6) ───────────────────────────────
    for (f, v2) in [
        ("has_doc", "true"),
        ("has_doc", "false"),
        ("is_recursive", "true"),
        ("is_recursive", "false"),
        ("has_fallthrough", "true"),
        ("has_fallthrough", "false"),
    ] {
        v.push((format!("g10_field_{f}_{v2}"), field_eq(f, v2)));
    }

    // ── Group 11: Enrichment field != value (4) ──────────────────────────────
    for (f, v2) in [
        ("has_doc", "true"),
        ("has_doc", "false"),
        ("is_recursive", "true"),
        ("is_recursive", "false"),
    ] {
        v.push((format!("g11_field_ne_{f}_{v2}"), field_ne(f, v2)));
    }

    // ── Group 12: Line numeric predicates (8) ────────────────────────────────
    for (label, c) in [
        ("gt_0", line_gt(0)),
        ("gt_10", line_gt(10)),
        ("gt_30", line_gt(30)),
        ("ge_1", line_ge(1)),
        ("ge_20", line_ge(20)),
        ("lt_50", line_lt(50)),
        ("lt_100", line_lt(100)),
        ("le_20", line_le(20)),
    ] {
        v.push((format!("g12_line_{label}"), c));
    }

    // ── Group 13: fql_kind + exact name (10) ─────────────────────────────────
    for (k, n) in [
        ("function", "foo"),
        ("function", "bar"),
        ("function", "factorial"),
        ("function", "noop"),
        ("function", "caller"),
        ("struct", "Motor"),
        ("enum", "State"),
        ("field", "speed"),
        ("variable", "count"),
        ("constant", "MAGIC"),
    ] {
        v.push((
            format!("g13_kind_{k}_name_{n}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                pred("name", CompareOp::Eq, n),
            ]),
        ));
    }

    // ── Group 14: fql_kind + LIKE (14) ───────────────────────────────────────
    for (k, p) in [
        ("function", "f%"),
        ("function", "b%"),
        ("function", "c%"),
        ("function", "s%"),
        ("function", "h%"),
        ("function", "d%"),
        ("function", "n%"),
        ("function", "t%"),
        ("function", "e%"),
        ("function", "%er"),
        ("function", "%ed"),
        ("struct", "M%"),
        ("enum", "S%"),
        ("variable", "c%"),
    ] {
        v.push((
            format!("g14_kind_{k}_like_{p}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                pred("name", CompareOp::Like, p),
            ]),
        ));
    }

    // ── Group 15: fql_kind + enrichment (8) ──────────────────────────────────
    for (k, f, val) in [
        ("function", "has_doc", "true"),
        ("function", "has_doc", "false"),
        ("function", "is_recursive", "true"),
        ("function", "is_recursive", "false"),
        ("struct", "has_doc", "true"),
        ("struct", "has_doc", "false"),
        ("enum", "has_doc", "true"),
        ("enum", "has_doc", "false"),
    ] {
        v.push((
            format!("g15_kind_{k}_{f}_{val}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                pred(f, CompareOp::Eq, val),
            ]),
        ));
    }

    // ── Group 16: fql_kind + line range (6) ──────────────────────────────────
    for (k, label, line_c) in [
        ("function", "gt_5", line_gt(5)),
        ("function", "gt_20", line_gt(20)),
        ("function", "lt_50", line_lt(50)),
        ("variable", "gt_0", line_gt(0)),
        ("struct", "gt_0", line_gt(0)),
        ("enum", "gt_0", line_gt(0)),
    ] {
        let c = add_pred_str(line_c, "fql_kind", CompareOp::Eq, k);
        v.push((format!("g16_kind_{k}_line_{label}"), c));
    }

    // ── Group 17: LIKE + enrichment (8) ──────────────────────────────────────
    for (p, f, val) in [
        ("f%", "has_doc", "true"),
        ("f%", "has_doc", "false"),
        ("b%", "has_doc", "true"),
        ("c%", "has_doc", "true"),
        ("s%", "has_doc", "true"),
        ("h%", "is_recursive", "false"),
        ("n%", "has_doc", "false"),
        ("t%", "has_doc", "true"),
    ] {
        v.push((
            format!("g17_like_{p}_{f}_{val}"),
            with_preds(vec![
                pred("name", CompareOp::Like, p),
                pred(f, CompareOp::Eq, val),
            ]),
        ));
    }

    // ── Group 18: ORDER BY field ASC (5) ─────────────────────────────────────
    for f in ["name", "line", "usages", "fql_kind", "language"] {
        v.push((format!("g18_order_asc_{f}"), order_asc(f)));
    }

    // ── Group 19: ORDER BY field DESC (5) ────────────────────────────────────
    for f in ["name", "line", "usages", "fql_kind", "language"] {
        v.push((format!("g19_order_desc_{f}"), order_desc(f)));
    }

    // ── Group 20: fql_kind + ORDER BY (10) ───────────────────────────────────
    for (k, f, dir) in [
        ("function", "name", SortDirection::Asc),
        ("function", "name", SortDirection::Desc),
        ("function", "line", SortDirection::Asc),
        ("function", "line", SortDirection::Desc),
        ("function", "usages", SortDirection::Asc),
        ("struct", "name", SortDirection::Asc),
        ("enum", "name", SortDirection::Asc),
        ("variable", "name", SortDirection::Asc),
        ("variable", "line", SortDirection::Asc),
        ("constant", "name", SortDirection::Asc),
    ] {
        let dir_str = if dir == SortDirection::Asc {
            "asc"
        } else {
            "desc"
        };
        v.push((
            format!("g20_kind_{k}_order_{f}_{dir_str}"),
            add_order(kind_eq(k), f, dir),
        ));
    }

    // ── Group 21: LIKE + ORDER BY (10) ───────────────────────────────────────
    for (p, f, dir) in [
        ("f%", "name", SortDirection::Asc),
        ("f%", "line", SortDirection::Asc),
        ("b%", "name", SortDirection::Asc),
        ("c%", "name", SortDirection::Asc),
        ("s%", "name", SortDirection::Asc),
        ("h%", "name", SortDirection::Asc),
        ("d%", "line", SortDirection::Asc),
        ("%er", "name", SortDirection::Asc),
        ("%ed", "name", SortDirection::Asc),
        ("n%", "line", SortDirection::Asc),
    ] {
        let dir_str = if dir == SortDirection::Asc {
            "asc"
        } else {
            "desc"
        };
        v.push((
            format!("g21_like_{p}_order_{f}_{dir_str}"),
            add_order(name_like(p), f, dir),
        ));
    }

    // ── Group 22: LIMIT=1000 (large, effectively no limit) (5) ──────────────
    for (label, c) in [
        ("all", all()),
        ("function", kind_eq("function")),
        ("like_f", name_like("f%")),
        ("has_doc_true", field_eq("has_doc", "true")),
        ("name_eq_foo", name_eq("foo")),
    ] {
        v.push((format!("g22_limit1000_{label}"), add_limit(c, 1000)));
    }

    // ── Group 23: LIMIT=1000 + ORDER BY (8) ──────────────────────────────────
    for (label, c, f, dir) in [
        ("all_name_asc", all(), "name", SortDirection::Asc),
        ("all_name_desc", all(), "name", SortDirection::Desc),
        ("all_line_asc", all(), "line", SortDirection::Asc),
        (
            "fn_name_asc",
            kind_eq("function"),
            "name",
            SortDirection::Asc,
        ),
        (
            "fn_line_asc",
            kind_eq("function"),
            "line",
            SortDirection::Asc,
        ),
        (
            "like_f_name_asc",
            name_like("f%"),
            "name",
            SortDirection::Asc,
        ),
        (
            "like_c_line_asc",
            name_like("c%"),
            "line",
            SortDirection::Asc,
        ),
        (
            "has_doc_name_asc",
            field_eq("has_doc", "true"),
            "name",
            SortDirection::Asc,
        ),
    ] {
        let dir_str = if dir == SortDirection::Asc {
            "asc"
        } else {
            "desc"
        };
        v.push((
            format!("g23_lim1000_{label}_order_{f}_{dir_str}"),
            add_limit(add_order(c, f, dir), 1000),
        ));
    }

    // ── Group 24: OFFSET + large LIMIT (8) ───────────────────────────────────
    for (off, _label) in [(0usize, "off0"), (5, "off5"), (10, "off10"), (20, "off20")] {
        for (base_label, c) in [("all", all()), ("fn", kind_eq("function"))] {
            let c = add_offset(add_order(c, "name", SortDirection::Asc), off);
            let c = add_limit(c, 1000);
            v.push((format!("g24_off{off}_{base_label}"), c));
        }
    }

    // ── Group 25: Triple combos — kind + LIKE + enrichment (8) ───────────────
    for (k, p, f, val) in [
        ("function", "f%", "has_doc", "true"),
        ("function", "f%", "has_doc", "false"),
        ("function", "b%", "has_doc", "true"),
        ("function", "c%", "is_recursive", "false"),
        ("function", "s%", "has_doc", "true"),
        ("function", "h%", "has_doc", "true"),
        ("function", "n%", "has_doc", "false"),
        ("function", "t%", "is_recursive", "true"),
    ] {
        v.push((
            format!("g25_{k}_{p}_{f}_{val}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                pred("name", CompareOp::Like, p),
                pred(f, CompareOp::Eq, val),
            ]),
        ));
    }

    // ── Group 26: kind + NOT LIKE (6) ─────────────────────────────────────────
    for (k, p) in [
        ("function", "f%"),
        ("function", "b%"),
        ("function", "%er"),
        ("function", "%ed"),
        ("variable", "c%"),
        ("struct", "nonexistent%"),
    ] {
        v.push((
            format!("g26_kind_{k}_not_like_{p}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                pred("name", CompareOp::NotLike, p),
            ]),
        ));
    }

    // ── Group 27: name + enrichment (6) ──────────────────────────────────────
    for (n, f, val) in [
        ("foo", "has_doc", "true"),
        ("foo", "has_doc", "false"),
        ("bar", "has_doc", "true"),
        ("factorial", "is_recursive", "true"),
        ("noop", "has_doc", "false"),
        ("Motor", "has_doc", "true"),
    ] {
        v.push((
            format!("g27_name_{n}_{f}_{val}"),
            with_preds(vec![
                pred("name", CompareOp::Eq, n),
                pred(f, CompareOp::Eq, val),
            ]),
        ));
    }

    // ── Group 28: line range + kind (6) ──────────────────────────────────────
    for (k, op, n, label) in [
        ("function", CompareOp::Gt, 0i64, "gt0"),
        ("function", CompareOp::Gt, 10, "gt10"),
        ("function", CompareOp::Lt, 80, "lt80"),
        ("function", CompareOp::Gte, 5, "ge5"),
        ("variable", CompareOp::Gt, 0, "gt0"),
        ("enum", CompareOp::Gt, 0, "gt0"),
    ] {
        v.push((
            format!("g28_{k}_line_{label}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                num_pred("line", op, n),
            ]),
        ));
    }

    // ── Group 29: more name combos + ORDER BY (8) ─────────────────────────────
    for (n, f, dir) in [
        ("foo", "line", SortDirection::Asc),
        ("bar", "line", SortDirection::Asc),
        ("factorial", "name", SortDirection::Asc),
        ("caller", "line", SortDirection::Asc),
        ("shadowed", "line", SortDirection::Asc),
        ("noop", "name", SortDirection::Asc),
        ("Motor", "name", SortDirection::Asc),
        ("MAGIC", "line", SortDirection::Asc),
    ] {
        let dir_str = if dir == SortDirection::Asc {
            "asc"
        } else {
            "desc"
        };
        v.push((
            format!("g29_name_{n}_order_{f}_{dir_str}"),
            add_order(name_eq(n), f, dir),
        ));
    }

    // ── Group 30: Multiple enrichment predicates (6) ──────────────────────────
    for (f1, v1, f2, v2) in [
        ("has_doc", "true", "is_recursive", "true"),
        ("has_doc", "true", "is_recursive", "false"),
        ("has_doc", "false", "is_recursive", "false"),
        ("has_doc", "true", "has_fallthrough", "false"),
        ("has_doc", "false", "has_fallthrough", "false"),
        ("is_recursive", "true", "has_fallthrough", "false"),
    ] {
        v.push((
            format!("g30_{f1}_{v1}_{f2}_{v2}"),
            with_preds(vec![
                pred(f1, CompareOp::Eq, v1),
                pred(f2, CompareOp::Eq, v2),
            ]),
        ));
    }

    // ── Group 31: kind + line range + ORDER BY (6) ────────────────────────────
    for (k, n, f, dir) in [
        ("function", 0i64, "line", SortDirection::Asc),
        ("function", 10, "line", SortDirection::Asc),
        ("function", 0, "name", SortDirection::Asc),
        ("variable", 0, "line", SortDirection::Asc),
        ("struct", 0, "name", SortDirection::Asc),
        ("enum", 0, "name", SortDirection::Asc),
    ] {
        let dir_str = if dir == SortDirection::Asc {
            "asc"
        } else {
            "desc"
        };
        let c = with_preds(vec![
            pred("fql_kind", CompareOp::Eq, k),
            num_pred("line", CompareOp::Gt, n),
        ]);
        v.push((
            format!("g31_{k}_line_gt{n}_order_{f}_{dir_str}"),
            add_order(c, f, dir),
        ));
    }

    // ── Group 32: LIKE prefix + kind + ORDER BY (8) ───────────────────────────
    for (k, p, f) in [
        ("function", "f%", "name"),
        ("function", "b%", "name"),
        ("function", "c%", "line"),
        ("function", "s%", "name"),
        ("function", "h%", "line"),
        ("function", "n%", "name"),
        ("variable", "c%", "line"),
        ("struct", "M%", "name"),
    ] {
        v.push((
            format!("g32_kind_{k}_like_{p}_order_{f}"),
            add_order(
                with_preds(vec![
                    pred("fql_kind", CompareOp::Eq, k),
                    pred("name", CompareOp::Like, p),
                ]),
                f,
                SortDirection::Asc,
            ),
        ));
    }

    // ── Group 33: Exact name + kind (negative — no match) (5) ────────────────
    // These expect empty results (wrong kind for the name)
    for (k, n) in [
        ("struct", "foo"),     // foo is a function, not struct
        ("function", "Motor"), // Motor is a struct, not function
        ("variable", "MAGIC"), // MAGIC is constant, not variable
        ("enum", "speed"),     // speed is a field, not enum
        ("constant", "count"), // count is variable, not constant
    ] {
        v.push((
            format!("g33_empty_kind_{k}_name_{n}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                pred("name", CompareOp::Eq, n),
            ]),
        ));
    }

    // ── Group 34: Clearly empty queries (5) ───────────────────────────────────
    v.push((
        "g34_empty_name_xyz".into(),
        name_eq("nonexistent_xyz_abc_123"),
    ));
    v.push(("g34_empty_kind_xyz".into(), kind_eq("nonexistent_kind_xyz")));
    v.push(("g34_empty_like_zzz".into(), name_like("zzz_nomatch_%")));
    v.push(("g34_empty_line_gt9999".into(), line_gt(9999)));
    v.push(("g34_empty_line_lt0".into(), line_lt(0)));

    // ── Group 35: kind != + LIKE (5) ──────────────────────────────────────────
    for (k, p) in [
        ("function", "M%"),
        ("struct", "f%"),
        ("enum", "f%"),
        ("variable", "M%"),
        ("constant", "f%"),
    ] {
        v.push((
            format!("g35_kind_ne_{k}_like_{p}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::NotEq, k),
                pred("name", CompareOp::Like, p),
            ]),
        ));
    }

    // ── Group 36: kind + enrichment + ORDER BY (6) ────────────────────────────
    for (k, f, val, ord) in [
        ("function", "has_doc", "true", "name"),
        ("function", "has_doc", "false", "name"),
        ("function", "is_recursive", "true", "line"),
        ("function", "is_recursive", "false", "name"),
        ("struct", "has_doc", "true", "name"),
        ("enum", "has_doc", "true", "name"),
    ] {
        v.push((
            format!("g36_{k}_{f}_{val}_order_{ord}"),
            add_order(
                with_preds(vec![
                    pred("fql_kind", CompareOp::Eq, k),
                    pred(f, CompareOp::Eq, val),
                ]),
                ord,
                SortDirection::Asc,
            ),
        ));
    }

    // ── Group 37: name LIKE suffix + kind (5) ────────────────────────────────
    for (p, k) in [
        ("%er", "function"),
        ("%ed", "function"),
        ("%al", "function"),
        ("%or", "struct"),
        ("%e", "function"),
    ] {
        v.push((
            format!("g37_like_{p}_kind_{k}"),
            with_preds(vec![
                pred("name", CompareOp::Like, p),
                pred("fql_kind", CompareOp::Eq, k),
            ]),
        ));
    }

    // ── Group 38: More exact names (4 more canonical symbols) ────────────────
    for n in ["Running", "Stopped", "deeply_nested", "no_default"] {
        v.push((format!("g38_name_eq_{n}"), name_eq(n)));
    }

    // ── Group 39: usages numeric predicates (4) ────────────────────────────────
    for (op, n, label) in [
        (CompareOp::Gte, 0i64, "ge0"),
        (CompareOp::Gt, 0, "gt0"),
        (CompareOp::Lte, 100, "le100"),
        (CompareOp::Gte, 1, "ge1"),
    ] {
        v.push((
            format!("g39_usages_{label}"),
            with_preds(vec![num_pred("usages", op, n)]),
        ));
    }

    // ── Group 40: kind + usages predicates (4) ────────────────────────────────
    for (k, op, n) in [
        ("function", CompareOp::Gte, 0i64),
        ("function", CompareOp::Gt, 0),
        ("struct", CompareOp::Gte, 0),
        ("variable", CompareOp::Gte, 0),
    ] {
        v.push((
            format!("g40_{k}_usages_ge{n}"),
            with_preds(vec![
                pred("fql_kind", CompareOp::Eq, k),
                num_pred("usages", op, n),
            ]),
        ));
    }

    v
}

// ── failure formatting ────────────────────────────────────────────────────────

type FailureRow<'a> = (
    &'a str,
    Vec<(String, String, usize)>,
    Vec<(String, String, usize)>,
);

fn format_failures(failures: &[FailureRow<'_>]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for (label, legacy, columnar) in failures {
        let _ = write!(
            out,
            "\n  [{label}] legacy={} columnar={}\n",
            legacy.len(),
            columnar.len()
        );
        // Show first few differing rows for diagnostics
        let max = legacy.len().max(columnar.len()).min(5);
        for i in 0..max {
            let l = legacy
                .get(i)
                .map_or_else(|| "<none>".to_owned(), |t| format!("{t:?}"));
            let c = columnar
                .get(i)
                .map_or_else(|| "<none>".to_owned(), |t| format!("{t:?}"));
            if legacy.get(i) != columnar.get(i) {
                let _ = writeln!(out, "    row {i}: legacy={l} columnar={c}");
            }
        }
    }
    out
}

// ── gate test ─────────────────────────────────────────────────────────────────

#[test]
fn parity_full_corpus() {
    let corpus = corpus();
    assert!(
        corpus.len() >= 200,
        "corpus must have ≥200 queries, has {}",
        corpus.len()
    );

    let dual = DualBackend::build();

    let mut failures: Vec<FailureRow<'_>> = Vec::new();

    for (label, clauses) in &corpus {
        let legacy = dual.legacy_run(clauses);
        let columnar = dual.columnar_run(clauses);
        if legacy != columnar {
            failures.push((label.as_str(), legacy, columnar));
        }
    }

    assert!(
        failures.is_empty(),
        "{} parity failures (out of {} corpus queries):{}",
        failures.len(),
        corpus.len(),
        format_failures(&failures)
    );
}
