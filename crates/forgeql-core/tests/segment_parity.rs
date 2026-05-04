//! Phase 04 — Parity harness: `SegmentReader` vs legacy `SymbolTable`.
//!
//! For each canonical fixture (`canonical.cpp`, `canonical.rs`) this test:
//!
//! 1. Indexes the fixture with `index_file` + the canonical enricher set,
//!    producing a legacy `SymbolTable`.
//! 2. Builds a `SegmentBuilder` from every row in the table (same logic as
//!    `ShadowWriter::run`), flushes to a temp directory, and opens the result
//!    with `SegmentReader`.
//! 3. Canonicalises both result sets (sort by (name, fql_kind, line)) and
//!    asserts they are identical on the fields that segments store: `name`,
//!    `fql_kind`, `language`, `line`, and all enrichment fields.
//!
//! Also contains Issue 4: a Linux-only memory budget test that measures page
//! faults before and after a `find_symbols` call scoped to `fql_kind =
//! 'function'` and documents the baseline page-touch count.
//!
//! Run with:
//! ```
//! cargo test -p forgeql-core --test segment_parity
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::items_after_statements,
    clippy::doc_markdown,
    clippy::missing_panics_doc
)]

use std::path::{Path, PathBuf};

use forgeql_core::ast::enrich::default_enrichers;
use forgeql_core::ast::index::{SymbolTable, index_file};
use forgeql_core::ast::lang::{CppLanguageInline, LanguageSupport, RustLanguageInline};
use forgeql_core::ir::Clauses;
use forgeql_core::result::SymbolMatch;
use forgeql_core::storage::columnar::{SegmentBuilder, SegmentReader};
use tempfile::TempDir;

// ── fixtures ─────────────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/canonical")
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Index a canonical fixture and return the populated `SymbolTable`.
fn index_canonical(lang: &dyn LanguageSupport, filename: &str) -> SymbolTable {
    let path = fixtures_dir().join(filename);
    assert!(path.exists(), "fixture missing: {}", path.display());

    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.tree_sitter_language())
        .expect("set_language");

    let enrichers = default_enrichers();
    let mut table = SymbolTable::default();

    let count = index_file(&mut parser, &path, &mut table, &enrichers, lang, None, None)
        .expect("index_file should succeed");
    assert!(count > 0, "expected at least one indexed row");
    table
}

/// Build a segment from every row in `table` and return `(TempDir, segment_dir)`.
///
/// Replicates the `ShadowWriter` loop — this is the reference path for
/// checking that flush + open is a round-trip identity.
fn build_segment_from_table(table: &SymbolTable) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let seg_dir = tmp.path().join("seg");

    let content_id = [0xCC_u8; 20]; // stable dummy content ID for tests
    let mut builder = SegmentBuilder::new("test", &content_id);

    for row in &table.rows {
        #[allow(clippy::cast_possible_truncation)]
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

    builder.flush(&seg_dir).expect("flush");
    (tmp, seg_dir)
}

/// Flatten a legacy `SymbolTable` to a canonicalised vec of `(name, fql_kind, line)` tuples.
fn legacy_key_tuples(table: &SymbolTable) -> Vec<(String, String, usize)> {
    let mut v: Vec<_> = table
        .rows
        .iter()
        .map(|r| {
            (
                table.name_of(r).to_owned(),
                table.fql_kind_of(r).to_owned(),
                r.line,
            )
        })
        .collect();
    v.sort_unstable();
    v
}

/// Flatten `find_symbols` results to the same canonical key form.
fn columnar_key_tuples(results: &[SymbolMatch]) -> Vec<(String, String, usize)> {
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

/// Assert field parity between legacy and columnar for every row in `table`.
///
/// Checks:
///  1. Every non-empty legacy field appears in the columnar result with the
///     same value (forward check).
///  2. No phantom fields exist in the columnar result that are absent from
///     legacy (reverse check).
///  3. `reader.extra_field_str(key, row_id)` agrees with `fields[key]` in the
///     materialised `SymbolMatch`, catching divergence between the low-level
///     accessor and the `find_symbols` pipeline (accessor cross-check).
fn assert_fields_match(
    lang: &str,
    table: &SymbolTable,
    reader: &SegmentReader,
    columnar_results: &[SymbolMatch],
) {
    for row in &table.rows {
        let name = table.name_of(row);
        let legacy_fields = table.resolve_fields(&row.fields);
        if legacy_fields.is_empty() {
            continue;
        }

        // Find the matching columnar result(s) by (name, fql_kind, line).
        let fql_kind = table.fql_kind_of(row);
        let matching: Vec<_> = columnar_results
            .iter()
            .filter(|r| {
                r.name == name
                    && r.fql_kind.as_deref() == Some(fql_kind)
                    && r.line == Some(row.line)
            })
            .collect();

        // Unexpected non-unique match (same name+fql_kind+line, multiple columnar
        // rows) is valid: e.g. the literal `1` appears twice on the same line.
        // The key-set parity test in `run_parity` already validates that the
        // total count is correct. Skip per-field and accessor checks here to
        // avoid false failures caused by ambiguity about which row owns which
        // field value.
        if matching.len() != 1 {
            continue;
        }
        let col = &matching[0];

        // ── forward check: every non-empty legacy field must appear in columnar ──
        for (key, val) in &legacy_fields {
            // The columnar reader does not store empty-string field values.
            // An empty-string legacy value is equivalent to absent/None
            // in the columnar path, so skip those entries.
            if val.is_empty() {
                continue;
            }
            let columnar_val = col.fields.get(key).map(String::as_str);
            assert_eq!(
                columnar_val,
                Some(val.as_str()),
                "[{lang}] '{name}' field '{key}': legacy='{val}', columnar={columnar_val:?}"
            );
        }

        // ── reverse check: no phantom fields in columnar result ──
        let legacy_non_empty: std::collections::HashSet<&str> = legacy_fields
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| k.as_str())
            .collect();
        for key in col.fields.keys() {
            assert!(
                legacy_non_empty.contains(key.as_str()),
                "[{lang}] '{name}' columnar result has phantom field '{key}' \
                 not present in legacy row"
            );
        }

        // ── accessor cross-check: extra_field_str must agree with fields map ──
        let candidate_ids: Vec<u32> = reader
            .lookup_name(name)
            .into_iter()
            .filter(|&id| {
                reader.fql_kind_of(id) == fql_kind && reader.line_of(id) as usize == row.line
            })
            .collect();
        if candidate_ids.len() == 1 {
            let row_id = candidate_ids[0];
            for (key, val) in &legacy_fields {
                if val.is_empty() {
                    continue;
                }
                let accessor_val = reader.extra_field_str(key, row_id);
                assert_eq!(
                    accessor_val,
                    Some(val.as_str()),
                    "[{lang}] '{name}' field '{key}': extra_field_str={accessor_val:?} \
                     but find_symbols returned '{val}'"
                );
            }
        }
    }
}

// ── parity tests ──────────────────────────────────────────────────────────────

/// Run the full parity assertion for one language/fixture pair.
fn run_parity(lang: &dyn LanguageSupport, filename: &str) {
    let table = index_canonical(lang, filename);
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("SegmentReader::open");

    assert_eq!(
        reader.row_count,
        u32::try_from(table.rows.len()).expect("row count fits u32"),
        "[{filename}] row_count mismatch"
    );

    let all_clauses = Clauses::default();
    let results = reader
        .find_symbols(&all_clauses, Some(Path::new(filename)))
        .expect("find_symbols");

    // Key-set equality: every (name, fql_kind, line) in legacy must appear in columnar.
    let legacy_keys = legacy_key_tuples(&table);
    let columnar_keys = columnar_key_tuples(&results);
    assert_eq!(
        legacy_keys, columnar_keys,
        "[{filename}] symbol set mismatch between legacy and columnar"
    );

    // Deep field equality for enrichment fields.
    assert_fields_match("all", &table, &reader, &results);
}

#[test]
fn parity_cpp_canonical() {
    run_parity(&CppLanguageInline, "canonical.cpp");
}

#[test]
fn parity_rust_canonical() {
    run_parity(&RustLanguageInline, "canonical.rs");
}

/// WHERE fql_kind = 'function' produces the same symbol set in both backends.
#[test]
fn parity_filter_fql_kind_function_cpp() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};

    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("function".to_owned()),
        }],
        ..Clauses::default()
    };
    let results = reader.find_symbols(&clauses, None).expect("find");

    // All columnar results must have fql_kind == "function".
    for r in &results {
        assert_eq!(
            r.fql_kind.as_deref(),
            Some("function"),
            "prefilter returned non-function row: {}",
            r.name
        );
    }

    // Count must match legacy.
    let legacy_fn_count = table
        .rows
        .iter()
        .filter(|r| table.fql_kind_of(r) == "function")
        .count();
    assert_eq!(
        results.len(),
        legacy_fn_count,
        "function count mismatch (columnar={}, legacy={legacy_fn_count})",
        results.len()
    );
}

/// ORDER BY line ASC produces rows in ascending line order.
#[test]
fn parity_order_by_line_asc_cpp() {
    use forgeql_core::ir::{OrderBy, SortDirection};

    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        ..Clauses::default()
    };
    let results = reader.find_symbols(&clauses, None).expect("find");
    let lines: Vec<_> = results.iter().map(|r| r.line.unwrap_or(0)).collect();
    assert!(
        lines.windows(2).all(|w| w[0] <= w[1]),
        "not sorted ASC by line: {lines:?}"
    );
}

/// ORDER BY line DESC produces rows in strictly descending line order and is
/// the exact reverse of the ASC result (Gap 8 fix).
#[test]
fn parity_order_by_line_desc_cpp() {
    use forgeql_core::ir::{OrderBy, SortDirection};

    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    let clauses_desc = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Desc,
        }),
        ..Clauses::default()
    };
    let desc = reader.find_symbols(&clauses_desc, None).expect("find desc");
    let desc_lines: Vec<_> = desc.iter().map(|r| r.line.unwrap_or(0)).collect();
    assert!(
        desc_lines.windows(2).all(|w| w[0] >= w[1]),
        "not sorted DESC by line: {desc_lines:?}"
    );

    // Must be the exact reverse of ASC.
    let clauses_asc = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        ..Clauses::default()
    };
    let asc = reader.find_symbols(&clauses_asc, None).expect("find asc");
    let asc_lines: Vec<_> = asc.iter().map(|r| r.line.unwrap_or(0)).collect();
    let reversed: Vec<_> = asc_lines.iter().copied().rev().collect();
    assert_eq!(
        desc_lines, reversed,
        "DESC should be the exact reverse of ASC"
    );
}

/// WHERE name LIKE 'f%' exercises the residual (non-Roaring) filter path and
/// must return the same symbol set as filtering the legacy SymbolTable (Gap 7 fix).
#[test]
fn parity_like_name_cpp() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};

    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "name".to_owned(),
            op: CompareOp::Like,
            value: PredicateValue::String("f%".to_owned()),
        }],
        ..Clauses::default()
    };
    let columnar_results = reader.find_symbols(&clauses, None).expect("find");

    // Build expected set from legacy table: names starting with 'f'.
    let legacy_names: std::collections::BTreeSet<String> = table
        .rows
        .iter()
        .map(|r| table.name_of(r).to_owned())
        .filter(|n| n.to_ascii_lowercase().starts_with('f'))
        .collect();
    let columnar_names: std::collections::BTreeSet<String> =
        columnar_results.iter().map(|r| r.name.clone()).collect();
    assert_eq!(
        columnar_names, legacy_names,
        "LIKE 'f%' result mismatch (columnar vs legacy)"
    );
    assert!(
        !columnar_names.is_empty(),
        "expected at least one name starting with 'f'"
    );
}

/// `byte_start_of` and `byte_end_of` accessors return the same byte range as
/// the legacy `IndexRow.byte_range` for every row in the canonical fixture
/// (Gap 4 fix).
#[test]
fn parity_byte_ranges_cpp() {
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    for row in &table.rows {
        let name = table.name_of(row);
        let fql_kind = table.fql_kind_of(row);

        // Collect all row_ids that match (name, fql_kind, line).
        let matching_ids: Vec<u32> = reader
            .lookup_name(name)
            .into_iter()
            .filter(|&id| {
                reader.fql_kind_of(id) == fql_kind && reader.line_of(id) as usize == row.line
            })
            .collect();

        if matching_ids.is_empty() {
            // Missing row is caught by parity_cpp_canonical — skip here.
            continue;
        }

        // When multiple rows share (name, fql_kind, line) — e.g. the literal `1`
        // appearing twice on the same line — assert that at least one of them
        // carries the correct byte range so no range is silently dropped.
        let has_correct_range = matching_ids.iter().any(|&id| {
            reader.byte_start_of(id) as usize == row.byte_range.start
                && reader.byte_end_of(id) as usize == row.byte_range.end
        });
        assert!(
            has_correct_range,
            "byte range ({}, {}) not found among {} row(s) for \
             '{name}' ({fql_kind}) at line {}",
            row.byte_range.start,
            row.byte_range.end,
            matching_ids.len(),
            row.line
        );
    }
}

/// `SegmentReader::lookup_name` returns the correct row IDs for a known symbol.
#[test]
fn parity_lookup_name_cpp() {
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    // "bar" is defined at line 16 in the canonical fixture.
    let rows = reader.lookup_name("bar");
    assert!(!rows.is_empty(), "expected at least one row for 'bar'");
    // Every returned row must have name == "bar".
    for &row in &rows {
        assert_eq!(reader.name_of(row), "bar", "row {row} name mismatch");
    }
}

/// Enrichment fields written by `SegmentBuilder` are readable via
/// `extra_field_str` on the same row IDs returned by `lookup_name`.
#[test]
fn parity_enrichment_fields_cpp() {
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);
    let reader = SegmentReader::open(&seg_dir).expect("open");

    // "bar" has has_doc=true (from CommentEnricher).
    let rows = reader.lookup_name("bar");
    assert!(!rows.is_empty(), "bar not found");
    let has_doc_found = rows
        .iter()
        .any(|&row| reader.extra_field_str("has_doc", row) == Some("true"));
    assert!(
        has_doc_found,
        "'bar' should have has_doc=true in columnar segment"
    );

    // "factorial" has is_recursive=true (from RecursionEnricher).
    let fac_rows = reader.lookup_name("factorial");
    assert!(!fac_rows.is_empty(), "factorial not found");
    let is_recursive = fac_rows
        .iter()
        .any(|&row| reader.extra_field_str("is_recursive", row) == Some("true"));
    assert!(is_recursive, "'factorial' should have is_recursive=true");
}

// ── Issue 4: memory budget test (Linux only) ──────────────────────────────────

/// Measure the number of minor page faults introduced by a `find_symbols` call
/// scoped to `WHERE fql_kind = 'function'` on the canonical C++ fixture.
///
/// The Roaring bitmap prefilter should limit column access to:
///   - `header.bin` (always in the page cache after `open`)
///   - `postings_fql_kind.bin` (loaded eagerly at open)
///   - `col_name_id.bin`, `col_fql_kind_id.bin`, `col_line.bin`,
///     `col_usages_count.bin`, `col_language_id.bin`
///   - `strings_offsets.bin`, `strings_data.bin`
///   - `name.fst`, `name_postings.bin` (not touched by prefilter path)
///
/// Columns NOT touched for a `fql_kind`-only query:
///   - `col_byte_start.bin`, `col_byte_end.bin`
///   - enrichment `col_<field>.bin` files for rows outside the candidate set
///
/// The documented baseline below is the measured value on the canonical.cpp
/// fixture (≈ 140 rows as of Phase 04).  It serves as the Phase 08 benchmark
/// starting point.
///
/// **Baseline** (2026-05-04, storage-engine-phase4, x86_64 Linux):
/// See the assertion comment at the bottom of this test for the measured value.
#[cfg(target_os = "linux")]
#[test]
fn memory_budget_fql_kind_prefilter_cpp() {
    use forgeql_core::ir::{CompareOp, Predicate, PredicateValue};

    // ── build segment ──────────────────────────────────────────────────────
    let table = index_canonical(&CppLanguageInline, "canonical.cpp");
    let (_tmp, seg_dir) = build_segment_from_table(&table);

    // Open the reader (this loads postings + FST into memory; all mmap
    // regions are registered with the OS but NOT yet paged in).
    let reader = SegmentReader::open(&seg_dir).expect("open");

    // ── read baseline minor-fault count ───────────────────────────────────
    let baseline_faults = read_minor_faults();

    // ── execute the query ──────────────────────────────────────────────────
    let clauses = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("function".to_owned()),
        }],
        ..Clauses::default()
    };
    let results = reader.find_symbols(&clauses, None).expect("find");
    assert!(!results.is_empty(), "expected function rows");

    // ── measure delta ──────────────────────────────────────────────────────
    let after_faults = read_minor_faults();
    let delta = after_faults.saturating_sub(baseline_faults);
    // small number of pages — well under 600 minor faults on a cold mmap.
    //
    // This bound is generous enough to avoid flakiness across CI environments
    // with different kernel page-reclaim policies (baseline ≈ 232 × ~2.6 ≈ 600).
    // The actual measured delta is printed below and serves as the
    // Phase 08 benchmark starting point.
    //
    // BASELINE (documented 2026-05-04, storage-engine-phase4, x86_64 Linux):
    //   delta ≈ 232 faults for canonical.cpp with all enrichment columns.
    println!(
        "[memory_budget] minor page faults during find_symbols(fql_kind=function): {delta} \
         (baseline={baseline_faults}, after={after_faults})"
    );
    assert!(
        delta < 600,
        "unexpected page-fault spike: {delta} faults for a small canonical.cpp segment; \
         expected < 600 (mmap should only touch accessed column pages; baseline ≈ 232)"
    );
}

/// Read the minor page fault count for the current process from
/// `/proc/self/stat` (field 10, 0-indexed).
#[cfg(target_os = "linux")]
fn read_minor_faults() -> u64 {
    use std::io::Read;
    let mut content = String::new();
    let _bytes_read = std::fs::File::open("/proc/self/stat")
        .expect("/proc/self/stat not readable")
        .read_to_string(&mut content)
        .expect("read_to_string");

    // /proc/self/stat format:
    // pid (comm) state ppid pgroup session ...
    // Field 10 (0-indexed) is `minflt` — minor (soft) page faults.
    // The `(comm)` field can contain spaces and parentheses, but we can
    // split on the closing `)` to skip it safely.
    let after_comm = content
        .rsplit_once(')')
        .map_or(content.as_str(), |(_, rest)| rest);
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // After stripping the "(comm)" field, the remaining fields are:
    //   [0]=state [1]=ppid [2]=pgroup [3]=session [4]=tty_nr [5]=tpgid
    //   [6]=flags [7]=minflt [8]=cminflt ...
    // So minflt is at index 7 in the stripped list.
    fields.get(7).and_then(|s| s.parse().ok()).unwrap_or(0)
}
