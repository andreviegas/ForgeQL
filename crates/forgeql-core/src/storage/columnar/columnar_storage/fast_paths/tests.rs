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

/// A segment whose rows disagree on every field a query might order by, so a
/// top-K over them is a real choice and not a prefix of the stored order.
///
/// Two rows share a name, which is what puts `order_cmp`'s tie-breakers to
/// work rather than leaving the primary key to decide everything.
fn ranked_segment() -> (tempfile::TempDir, SegmentReader) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("ranked.fqsf");
    let content_id = [0x5B_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    let rows = [
        ("delta", 40, "3"),
        ("alpha", 10, "9"),
        ("charlie", 30, "1"),
        ("bravo", 20, "7"),
        ("alpha", 50, "5"),
        ("echo", 5, "2"),
        ("foxtrot", 60, "8"),
        ("golf", 15, "4"),
        ("hotel", 25, "6"),
    ];
    for (name, line, param_count) in rows {
        let row = b.emit_row(SymbolRow {
            name,
            fql_kind: "function",
            language: "rust",
            line,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        b.set_field(row, "param_count", param_count);
    }
    b.flush(&path).expect("flush");
    let reader = SegmentReader::open(&path).expect("open");
    (tmp, reader)
}

/// Choosing the page from row views must pick the rows that choosing it from
/// built rows picks.
///
/// This is the whole of what `ColumnarStorage::materialize_top_k` claims: it
/// ranks `SegRowRef`s so that the rows losing the ranking are never built, and
/// that is only the same query while a view orders exactly as the row it would
/// have become. The two are compared for real here rather than argued from the
/// fact that both call `order_cmp` — the comparator is shared, but the values
/// it reads come from two different readers, and a field where those two
/// disagree (an empty string read as absent on one side and present on the
/// other, say) would show up as a different page and nowhere else.
#[test]
fn choosing_by_row_view_picks_what_choosing_by_built_row_picks() {
    let (_tmp, seg) = ranked_segment();
    let source_path = Path::new("src/ranked.rs");
    let all: RoaringBitmap = (0..seg.row_count).collect();

    let orderings = [
        ("name", SortDirection::Asc),
        ("name", SortDirection::Desc),
        ("line", SortDirection::Asc),
        ("line", SortDirection::Desc),
        ("param_count", SortDirection::Asc),
        ("param_count", SortDirection::Desc),
        // Equal on every row, so the tie-breakers decide the page alone.
        ("fql_kind", SortDirection::Asc),
        // No column of this segment holds it, so both the view and the row it
        // builds rank every row by the same absence. This is the case a gate
        // written on answerability would have refused, taking almost every real
        // query with it.
        ("has_doc", SortDirection::Asc),
    ];

    for (field, direction) in orderings {
        let ascending = matches!(direction, SortDirection::Asc);
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: field.to_owned(),
                direction,
            }),
            ..Default::default()
        };
        assert!(
            segment_ranks_from_columns(&seg, field),
            "the fixture must be eligible for the view path, or this test \
             compares the built path with itself"
        );

        for k in [1_usize, 3, 5] {
            let views: Vec<SegRowRef<'_>> = all
                .iter()
                .map(|row| SegRowRef {
                    seg: &seg,
                    row,
                    source_path: Some(source_path),
                })
                .collect();
            let by_view: Vec<(String, Option<usize>)> =
                collect_top_k(views, k, |a, b| order_cmp(a, b, &clauses))
                    .iter()
                    .filter_map(|view| seg.materialize_one_row(view.row, source_path))
                    .map(|row| (row.name, row.line))
                    .collect();

            let built = seg.materialize_rows(&all, Some(source_path));
            let by_row: Vec<(String, Option<usize>)> =
                collect_top_k(built, k, |a, b| order_cmp(a, b, &clauses))
                    .into_iter()
                    .map(|row| (row.name, row.line))
                    .collect();

            assert_eq!(
                by_view.len(),
                k,
                "ORDER BY {field} {} LIMIT {k}: the view path returned a short page",
                if ascending { "ASC" } else { "DESC" }
            );
            assert_eq!(
                by_view,
                by_row,
                "ORDER BY {field} {} LIMIT {k}: ranking row views chose a \
                 different page from ranking the rows they build",
                if ascending { "ASC" } else { "DESC" }
            );
        }
    }
}

/// A segment is admitted to the column-ranked path only when it can rank by
/// every field the comparator reads.
///
/// Each rejection here is a wrong answer the gate is holding back rather than a
/// missed optimisation: `usages` is a stale zero in the column while the real
/// count arrives after materialisation, and the node handle is derived as the
/// row is built.
///
/// The admission of `has_doc` is the other half, and it matters just as much.
/// The gate was first written on `answers_field` — the predicate that decides
/// whether a WHERE can run early — and a field no column holds fails that. On a
/// real corpus most segments hold no column for any given enrichment field, so
/// that version admitted almost nothing. It was also decided once for the whole
/// query rather than per segment, which made it hostage to the worst segment
/// present: a single segment carrying enrichment columns named `name` and
/// `path` withheld those fields from a row view, and that one segment switched
/// the path off for every query in the workspace. Both faults passed the whole
/// suite. What found them was emptying the path's result and watching which
/// tests changed — one pre-existing case, and none of the ones written for it —
/// and then panicking inside the path to tell "never reached" apart from
/// "reached and overridden". Absence ranks the same on both sides, so it is
/// admitted, and this case is what says so.
#[test]
fn a_segment_that_cannot_rank_its_own_rows_is_kept_off_the_view_path() {
    let (_tmp, seg) = ranked_segment();

    assert!(segment_ranks_from_columns(&seg, "line"));
    assert!(segment_ranks_from_columns(&seg, "param_count"));
    assert!(
        segment_ranks_from_columns(&seg, "has_doc"),
        "no column here holds has_doc, so the view ranks every row by its \
         absence and so does every row it would build. Agreeing on nothing is \
         still agreeing, and rejecting this case is what would leave the whole \
         path dead on a real corpus, where most segments carry no column for \
         any given enrichment field"
    );

    assert!(
        !segment_ranks_from_columns(&seg, "usages"),
        "the usages column is a stale zero the workspace count replaces after \
         materialisation, so the view would rank by zero where the built row \
         ranks by the real count"
    );
    assert!(
        !segment_ranks_from_columns(&seg, "node_id"),
        "the node handle is derived from the row's ordinal as the row is built, \
         so the view would rank by nothing where the built row ranks by a handle"
    );
}

/// A residual predicate that only a built row can answer keeps the segment
/// building everything it matched.
///
/// The rank alone is not enough to discard a row while a filter it has not
/// faced is still to come: the row dropped might be the one that survived the
/// predicate. This is checked on `topk_rows_of_segment` rather than on the
/// segment, because it is a property of what the query left over for these
/// rows, not of the segment's columns.
#[test]
fn a_predicate_still_to_run_keeps_every_matched_row() {
    let (_tmp, seg) = ranked_segment();
    let source_path = Path::new("src/ranked.rs");
    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(1),
        ..Default::default()
    };

    let rows: RoaringBitmap = (0..seg.row_count).collect();
    let ready = NarrowedSegment {
        seg: &seg,
        source_path,
        rows: rows.clone(),
        late: Vec::new(),
    };
    let pruned = ColumnarStorage::topk_rows_of_segment(&ready, &clauses)
        .expect("nine rows is past K * TOPK_OVER_FETCH for K = 1");
    assert_eq!(
        pruned.len(),
        topk_keep(1) as u64,
        "the retained size is the trim's own, not a number this function picked"
    );

    let waiting = NarrowedSegment {
        seg: &seg,
        source_path,
        rows,
        late: vec![predicate(
            "has_doc",
            CompareOp::Eq,
            PredicateValue::String("true".to_owned()),
        )],
    };
    assert!(
        ColumnarStorage::topk_rows_of_segment(&waiting, &clauses).is_none(),
        "a predicate still to run must keep every matched row"
    );
}

/// Below the trim's own threshold nothing is shed, so the segment contributes
/// exactly what it did before.
///
/// This is what makes the change a cost change: the rows this sheds are rows
/// the running trim would have shed on the very next statement, and where the
/// trim would not have fired neither does this.
#[test]
fn nothing_is_shed_below_the_threshold_the_trim_uses() {
    let (_tmp, seg) = ranked_segment();
    let source_path = Path::new("src/ranked.rs");
    let rows: RoaringBitmap = (0..seg.row_count).collect();
    let matched = rows.len();

    for k in 1..=(seg.row_count as usize + 2) {
        let clauses = Clauses {
            order_by: Some(OrderBy {
                field: "line".to_owned(),
                direction: SortDirection::Asc,
            }),
            limit: Some(k),
            ..Default::default()
        };
        let narrowed = NarrowedSegment {
            seg: &seg,
            source_path,
            rows: rows.clone(),
            late: Vec::new(),
        };
        let fires = matched > (k * TOPK_OVER_FETCH) as u64;
        assert_eq!(
            ColumnarStorage::topk_rows_of_segment(&narrowed, &clauses).is_some(),
            fires,
            "LIMIT {k} over {matched} matched rows: this must shed exactly when \
             the running trim would"
        );
    }
}

/// The two row builders must produce the same row.
///
/// The build-then-trim path calls `materialize_rows`, which resolves each
/// column once for a whole batch; the pre-materialisation top-K calls
/// `materialize_one_row`, because by then it holds a handful of scattered rows
/// rather than a bitmap worth batching. Two builders for one row is exactly the
/// shape that drifts — one gains a field the other does not — and the drift
/// would show up as a query answering differently depending on which path took
/// it, which no test of either path alone can see.
///
/// Compared through `serde_json` rather than field by field so that a field
/// added to `SymbolMatch` later is covered without anyone remembering to add it
/// here.
#[test]
fn the_two_row_builders_build_the_same_row() {
    let (_tmp, seg) = ranked_segment();
    let source_path = Path::new("src/ranked.rs");
    let all: RoaringBitmap = (0..seg.row_count).collect();

    let batched = seg.materialize_rows(&all, Some(source_path));
    assert_eq!(batched.len(), seg.row_count as usize);

    for (row, batched_row) in all.iter().zip(&batched) {
        let single = seg
            .materialize_one_row(row, source_path)
            .unwrap_or_else(|| panic!("row {row} is inside the segment"));
        assert_eq!(
            serde_json::to_value(&single).expect("serialize"),
            serde_json::to_value(batched_row).expect("serialize"),
            "row {row} differs between materialize_one_row and materialize_rows"
        );
    }

    assert!(
        seg.materialize_one_row(seg.row_count, source_path)
            .is_none(),
        "a row index past the end must be declined, not built from whatever \
         the columns return for it"
    );
}

/// Every tie-breaker `order_cmp` publishes has to be answerable from a segment
/// before that segment may rank its own rows, not just the ORDER BY field.
///
/// The gate reads `ORDER_TIE_BREAKERS`, so a tie-breaker added to the list
/// without a segment being able to answer it must close the path rather than
/// silently rank by less.
#[test]
fn the_view_path_gate_covers_every_published_tie_breaker() {
    let (_tmp, seg) = ranked_segment();
    for field in crate::filter::ORDER_TIE_BREAKERS {
        assert!(
            seg.answers_field(field, true),
            "{field} is published as a tie-breaker but this segment cannot \
             answer it, so the gate would admit a ranking that reads it from \
             the built row only"
        );
        assert_eq!(
            seg.answers_field(field, true),
            seg.ranks_field_like_a_built_row(field, true),
            "every tie-breaker is a struct-backed name, so the two predicates \
             must coincide on it. That is what keeps the relaxation honest: \
             admitting a field no column holds must not also admit a \
             struct-backed name a column has shadowed, which reads as absent \
             on the view while the built row still reports its own field"
        );
    }
    assert!(
        !seg.answers_field("path", false),
        "with no source path there is nothing to answer `path` with, which is \
         why the gate asks with has_path = true"
    );
}

/// Every field the gate admits must read the same on a row view as on the row
/// that view would build.
///
/// `segment_orders_from_columns` is a promise about two readers; this checks
/// the promise against the readers rather than restating it. The page-level
/// test above would catch a divergence only where it happens to change the
/// ranking, which a field that ties on every fixture row never does — so a
/// divergence could sit in `language` or `path` indefinitely and surface on a
/// corpus instead.
///
/// The list deliberately includes `has_doc`, which no column here holds: that
/// field is admitted precisely because both sides report absence, and it is the
/// admission the gate turns on.
#[test]
fn every_admitted_field_reads_the_same_on_a_view_as_on_the_row_it_builds() {
    use crate::filter::ClauseTarget as _;

    let (_tmp, seg) = ranked_segment();
    let source_path = Path::new("src/ranked.rs");
    let all: RoaringBitmap = (0..seg.row_count).collect();
    let built = seg.materialize_rows(&all, Some(source_path));

    for field in [
        "name",
        "line",
        "path",
        "fql_kind",
        "language",
        "param_count",
        "has_doc",
    ] {
        assert!(
            seg.ranks_field_like_a_built_row(field, true),
            "{field} is not admitted, so this loop would be checking a field \
             the gate never lets through"
        );
        for (row, built_row) in all.iter().zip(&built) {
            let view = SegRowRef {
                seg: &seg,
                row,
                source_path: Some(source_path),
            };
            assert_eq!(
                view.field_str(field),
                built_row.field_str(field),
                "row {row}: {field} reads differently as a string on the view \
                 and on the row it builds"
            );
            assert_eq!(
                view.field_num(field),
                built_row.field_num(field),
                "row {row}: {field} reads differently as a number on the view \
                 and on the row it builds"
            );
        }
    }

    // And the counterpart: a field the gate refuses is refused because the two
    // readers really do disagree, not out of caution. `usages` is the one that
    // matters most, since ORDER BY usages is a documented recipe.
    assert!(!seg.ranks_field_like_a_built_row("usages", true));
    let view = SegRowRef {
        seg: &seg,
        row: 0,
        source_path: Some(source_path),
    };
    assert_eq!(view.field_num("usages"), None);
    assert_eq!(built[0].field_num("usages"), Some(0));
}
