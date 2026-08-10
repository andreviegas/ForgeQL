//! Tests for the module-level helpers in [`super`].

use super::*;
use crate::ir::Predicate;
use crate::storage::columnar::segment_builder::{SegmentBuilder, SymbolRow};

/// One row carrying one enrichment column, enough for `answers_field` to say
/// yes to `param_count` and no to everything the segment does not hold.
fn one_row_segment() -> (tempfile::TempDir, SegmentReader) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("seg.fqsf");
    let content_id = [0x2A_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    let row = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 12,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    b.set_field(row, "param_count", "2");
    b.flush(&path).expect("flush");
    let reader = SegmentReader::open(&path).expect("open");
    (tmp, reader)
}

fn predicate(field: &str, op: CompareOp, value: PredicateValue) -> Predicate {
    Predicate {
        field: field.to_owned(),
        op,
        value,
    }
}

/// A predicate that cannot be answered before materialisation must be handed
/// to the filter that runs after it — never quietly left out of both. A
/// dropped predicate does not fail loudly: it returns rows the query excluded,
/// which reads as a successful answer. So the split is checked for totality,
/// not only for putting the right predicates on the fast side.
#[test]
fn every_predicate_lands_on_exactly_one_side_of_the_split() {
    let (_tmp, reader) = one_row_segment();
    let predicates = vec![
        predicate("line", CompareOp::Gte, PredicateValue::Number(10)),
        predicate("param_count", CompareOp::Eq, PredicateValue::Number(2)),
        // Stamped from the workspace overlay only after materialisation.
        predicate("usages", CompareOp::Gte, PredicateValue::Number(1)),
        // No column of this segment holds it.
        predicate(
            "has_doc",
            CompareOp::Eq,
            PredicateValue::String("true".to_owned()),
        ),
        // Cheaper compiled once for a whole batch than once per row.
        predicate(
            "name",
            CompareOp::Matches,
            PredicateValue::String("^al".to_owned()),
        ),
    ];

    let (early, late) = split_seg_predicates(&reader, &predicates, true);

    assert_eq!(
        early.len() + late.len(),
        predicates.len(),
        "the split must not lose a predicate"
    );
    let mut got: Vec<&str> = early.iter().map(|(field, _)| *field).collect();
    got.extend(late.iter().map(|p| p.field.as_str()));
    got.sort_unstable();
    let mut want: Vec<&str> = predicates.iter().map(|p| p.field.as_str()).collect();
    want.sort_unstable();
    assert_eq!(got, want, "the two halves together must be the input");

    let early_fields: Vec<&str> = early.iter().map(|(field, _)| *field).collect();
    assert_eq!(early_fields, ["line", "param_count"]);
    let late_fields: Vec<&str> = late.iter().map(|p| p.field.as_str()).collect();
    assert_eq!(late_fields, ["usages", "has_doc", "name"]);
}

/// With no path for the segment there is nothing to answer `path` with, so it
/// joins the late half rather than resolving to nothing on every row.
#[test]
fn a_path_predicate_waits_for_the_rows_when_the_caller_supplied_no_path() {
    let (_tmp, reader) = one_row_segment();
    let predicates = vec![predicate(
        "path",
        CompareOp::Like,
        PredicateValue::String("src/%".to_owned()),
    )];

    let (early, late) = split_seg_predicates(&reader, &predicates, false);
    assert!(early.is_empty());
    assert_eq!(late.len(), 1);

    let (early, late) = split_seg_predicates(&reader, &predicates, true);
    assert_eq!(early.len(), 1);
    assert!(late.is_empty());
}

/// The row bound is the memory budget divided by the per-row cost, not a
/// number someone picked.
///
/// This is the property that rotted: the bound was a bare `5_000_000` that
/// outlived several growths of the result row, so by the time anyone checked it
/// authorised roughly 7.5 GB against a stated budget of 2 GiB. Pinning the
/// derivation means the next growth of `FIND_BYTES_PER_ROW` moves the bound
/// instead of quietly widening what a query may spend.
#[test]
fn the_row_bound_is_derived_from_the_memory_budget() {
    let spend = super::DEFAULT_FIND_MAX_ROWS * super::FIND_BYTES_PER_ROW;
    assert!(
        spend <= super::FIND_ROW_BUDGET_BYTES,
        "the bound authorises {spend} bytes against a budget of {}",
        super::FIND_ROW_BUDGET_BYTES
    );

    // And it is the *largest* count that fits, which is what tells a derived
    // bound apart from any smaller number that would also pass the line above.
    let one_more = (super::DEFAULT_FIND_MAX_ROWS + 1) * super::FIND_BYTES_PER_ROW;
    assert!(
        one_more > super::FIND_ROW_BUDGET_BYTES,
        "one row more than the bound still fits the budget, so the bound is not \
         derived from it: {one_more} <= {}",
        super::FIND_ROW_BUDGET_BYTES
    );
}
