//! Tests for the module-level helpers in [`super`].
//!
//! These call the helpers directly, and the bounded per-segment choice is
//! called here with `topk_trim_for` — the ungated budget — on purpose, so that
//! each case sets the value it is testing against. That is deliberate, and it
//! has a cost worth stating: what the engine passes is `trim_budget`, which
//! first refuses to shed at all when the overlay reports duplicate source
//! paths, and no case in this file can observe that refusal. The one that can
//! is `duplicate_paths_disarm_the_shedders_so_the_count_stays_honest` in
//! `tests/topk_trim_before_dedupe.rs`; it needs two segments over one path to
//! exist before it can ask anything, which is why it lives outside this file.

use super::*;
use crate::ir::Predicate;
use crate::storage::columnar::segment_builder::{SegmentBuilder, SymbolRow};
use crate::storage::columnar::segment_reader::Accessor;

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
        // No column of this segment holds it, and the row it would build would
        // not carry it either — so both readers answer None and this is
        // decided here, not after the rows are built.
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
    assert_eq!(early_fields, ["line", "param_count", "has_doc"]);
    let late_fields: Vec<&str> = late.iter().map(|p| p.field.as_str()).collect();
    assert_eq!(late_fields, ["usages", "name"]);
}

/// A predicate on a field no column of the segment holds is answered early,
/// and it answers what the built row's filter would have answered.
///
/// This is the arm that is easiest to get wrong, so it is checked against the
/// built path rather than against an expectation. For each operator the same
/// predicate is run two ways: over a `SegRowRef`, which is what the early
/// filter does, and over the `SymbolMatch` the same row materialises into,
/// which is what `apply_where_predicates` falls through to for every operator
/// that is not a regex. The two must agree for every row.
///
/// The negative operators are the point. `!=` and `NOT LIKE` on an absent
/// field are **false**, not true — every arm of `eval_predicate_on` is
/// `is_some_and`, so a missing value fails the operator rather than passing
/// it — and a reading that "a negative predicate lets every row through"
/// would silently add rows the built path drops.
#[test]
fn an_absent_field_answers_early_exactly_as_the_built_row_answers_it() {
    let (_tmp, seg) = ranked_segment();
    let source = std::path::Path::new("ranked.rs");
    let rows = seg.materialize_rows(&(0..seg.row_count).collect(), Some(source));

    // `has_doc` is on no column of this fixture, so nothing on the row or in
    // its enrichment map carries it.
    let cases = [
        predicate(
            "has_doc",
            CompareOp::Eq,
            PredicateValue::String("true".into()),
        ),
        predicate(
            "has_doc",
            CompareOp::NotEq,
            PredicateValue::String("true".into()),
        ),
        predicate(
            "has_doc",
            CompareOp::Like,
            PredicateValue::String("tr%".into()),
        ),
        predicate(
            "has_doc",
            CompareOp::NotLike,
            PredicateValue::String("tr%".into()),
        ),
        predicate("has_doc", CompareOp::Eq, PredicateValue::Number(1)),
        predicate("has_doc", CompareOp::NotEq, PredicateValue::Number(1)),
        predicate("has_doc", CompareOp::Gt, PredicateValue::Number(0)),
        predicate("has_doc", CompareOp::Gte, PredicateValue::Number(0)),
        predicate("has_doc", CompareOp::Lt, PredicateValue::Number(99)),
        predicate("has_doc", CompareOp::Lte, PredicateValue::Number(99)),
        // Passes every row without ever consulting the field, on both readers.
        predicate("has_doc", CompareOp::NotLike, PredicateValue::Number(5)),
    ];

    for pred in &cases {
        assert!(
            !predicate_waits_for_a_built_row(&seg, pred, true),
            "{:?} on an absent field should be answered early",
            pred.op
        );
        for row in 0..seg.row_count {
            let view = crate::storage::columnar::segment_reader::SegRowRef {
                seg: &seg,
                row,
                source_path: Some(source),
            };
            assert_eq!(
                crate::filter::eval_predicate(&view, pred),
                crate::filter::eval_predicate(&rows[row as usize], pred),
                "{:?} disagrees with the built row on row {row}",
                pred.op
            );
        }
    }

    // Every operator that consults the field is false; only the arm that
    // short-circuits before reading it passes.
    let view = crate::storage::columnar::segment_reader::SegRowRef {
        seg: &seg,
        row: 0,
        source_path: Some(source),
    };
    for pred in &cases[..cases.len() - 1] {
        assert!(
            !crate::filter::eval_predicate(&view, pred),
            "{:?} on an absent field must be false, negations included",
            pred.op
        );
    }
    assert!(crate::filter::eval_predicate(
        &view,
        &cases[cases.len() - 1]
    ));
}

/// "No column holds it" is only "nothing holds it" for a field nothing writes
/// onto the row afterwards, and this is the guard that says so.
///
/// `field_tiers` declares `body` and `role` as written after the columns are
/// read — `body` out of the file as the row is materialised, `role` by the read
/// pass that finds an occurrence site. A reader looking only at the columns
/// sees nothing for either and would conclude a confident absence the built row
/// may contradict, so both keep waiting for the row. Nothing writes either on
/// the on-disk `FIND symbols` path today, which is exactly why the rule has to
/// ask `field_tiers` rather than observe `materialize_rows`: the invariant is
/// held by a declaration, not by what one function currently happens to fill in.
#[test]
fn a_field_written_after_materialisation_is_not_treated_as_absent() {
    let (_tmp, seg) = ranked_segment();

    for field in ["body", "role"] {
        assert!(
            crate::field_tiers::written_after_materialisation(field),
            "{field} is the case this guard exists for"
        );
        assert!(
            predicate_waits_for_a_built_row(
                &seg,
                &predicate(field, CompareOp::Eq, PredicateValue::String("x".into())),
                true
            ),
            "{field} has no column but is written after materialisation, so it \
             must wait for the row"
        );
    }

    assert!(
        !crate::field_tiers::written_after_materialisation("has_doc"),
        "an ordinary enrichment name must not be caught by the guard, or every \
         absent field goes back to waiting and the route closes again"
    );
    assert!(!predicate_waits_for_a_built_row(
        &seg,
        &predicate(
            "has_doc",
            CompareOp::Eq,
            PredicateValue::String("true".into())
        ),
        true
    ));

    // A regex still waits, and not only for cost: on a value that resolves to
    // None the batch filter KEEPS a NOT MATCHES row while a per-row evaluation
    // would drop it, so letting one through here would move the answer.
    assert!(predicate_waits_for_a_built_row(
        &seg,
        &predicate(
            "has_doc",
            CompareOp::NotMatches,
            PredicateValue::String("^t".into())
        ),
        true
    ));
    assert!(predicate_waits_for_a_built_row(
        &seg,
        &predicate(
            "has_doc",
            CompareOp::Matches,
            PredicateValue::String("^t".into())
        ),
        true
    ));
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

/// The bound on views is derived from the same budget, and a view costs what
/// the docs say it costs.
///
/// The size is read off the type rather than guessed at, so this cannot drift
/// from the struct — but it CAN drift from the prose, in two separate ways that
/// do not overlap.
///
/// The BYTE figure is quoted in nine places: `FIND_BYTES_PER_VIEW`'s own
/// comment, `DEFAULT_FIND_MAX_VIEWS`, `doc/syntax.md`, `doc/architecture.md`,
/// the four agent documents, and the changelog, all of which say "48 bytes"
/// and "about 44.7 million". The changelog is one site with two spellings: a
/// live fragment in `changelog.d/` before the release that carries it, and the
/// assembled entry in `CHANGELOG.md` after — a number left stale in either is
/// published rather than merely wrong, so check for both.
///
/// The RATIO against a built row is quoted in three more, and the byte figure
/// appears in none of them, so nobody working the list above would open them:
/// [`super::DEFAULT_FIND_MAX_ROWS`] ("a thirty-third of the size"),
/// `page_from_row_views` ("about a thirty-third of its size") and
/// `carried_row_budget_exceeded` ("roughly thirty times looser"). The last of
/// those interpolates the byte constants and hand-writes the ratio, so it is
/// half safe and half not — which is exactly the kind of site a shorter list
/// would have discharged wrongly.
///
/// Growing the view is legal; shipping it with any of those twelve sentences
/// still claiming the old number is not, and this is what says so.
#[test]
fn a_view_costs_what_the_scan_bound_prices_it_at() {
    assert_eq!(
        super::FIND_BYTES_PER_VIEW,
        48,
        "a row view has changed size. Two figures go stale, not one. The BYTE \
         figure is quoted in FIND_BYTES_PER_VIEW, DEFAULT_FIND_MAX_VIEWS, \
         doc/syntax.md, doc/architecture.md, the four agent documents, and the \
         changelog — as a live fragment under changelog.d/ before the release \
         that carries it, and as the assembled entry in CHANGELOG.md after; \
         a stale number in either is published. The RATIO against a \
         built row is quoted separately, and in places the byte figure is not: \
         DEFAULT_FIND_MAX_ROWS ('a thirty-third of the size'), \
         page_from_row_views ('about a thirty-third of its size') and \
         carried_row_budget_exceeded ('roughly thirty times looser'), which \
         interpolates the byte figures but hand-writes that ratio. Update both."
    );
    const {
        assert!(
            super::FIND_BYTES_PER_VIEW < super::FIND_BYTES_PER_ROW,
            "a view that cost as much as the row it stands for would make \
             choosing a page before building it pointless"
        );
        assert!(
            super::DEFAULT_FIND_MAX_VIEWS > super::DEFAULT_FIND_MAX_ROWS,
            "the two bounds share a budget and differ only in what a row costs \
             on each side of it, so the carried one must be the looser"
        );
    }

    let spend = super::DEFAULT_FIND_MAX_VIEWS * super::FIND_BYTES_PER_VIEW;
    assert!(
        spend <= super::FIND_ROW_BUDGET_BYTES,
        "the view bound authorises {spend} bytes against a budget of {}",
        super::FIND_ROW_BUDGET_BYTES
    );
    let one_more = (super::DEFAULT_FIND_MAX_VIEWS + 1) * super::FIND_BYTES_PER_VIEW;
    assert!(
        one_more > super::FIND_ROW_BUDGET_BYTES,
        "one view more than the bound still fits the budget, so the bound is \
         not derived from it: {one_more} <= {}",
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
/// This is the whole of what `ColumnarStorage::topk_rows_of_segment` claims: it
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
        // builds rank every row by the same absence — and, since that same
        // agreement is now what lets the WHERE answer it early, decide it the
        // same way too. The ranking admitted this before the predicate reader
        // did; they no longer differ on it.
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
            ordering_travels_on_views(field),
            "the ordering must be eligible for the view path, or this test \
             compares the built path with itself"
        );

        for k in [1_usize, 3, 5] {
            let views: Vec<RowView<'_>> = all
                .iter()
                .map(|row| RowView::of(&seg, Some(source_path), row))
                .collect();
            let by_view: Vec<(String, Option<usize>)> =
                collect_top_k(views, k, |a, b| order_cmp(a, b, &clauses))
                    .iter()
                    .filter_map(RowView::materialize)
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

/// An ordering rides row views only when a view ranks by every field the
/// comparator reads.
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
/// that version admitted almost nothing.
///
/// The admission of a *shadowed* struct-backed name is the third half, and it
/// was the expensive one. The gate used to refuse a segment carrying an
/// enrichment column named `name` or `path`, on the ground that the view
/// reported the field absent while the built row still answered from its own
/// struct. It was cheaper to make the view read the struct's column: measured
/// on this repository, 308 of 411 segments carry a column called `name` —
/// `extract_fields` writes every tree-sitter grammar field of every emitted
/// node as an enrichment column, and `name` is a grammar field on essentially
/// every definition node — so the refusal took three quarters of every scan off
/// the cheap route. That is also why the question is no longer asked of a
/// segment at all: nothing about which file a segment came from bears on it.
///
/// Both earlier faults passed the whole suite. What found them was emptying the
/// path's result and watching which tests changed — one pre-existing case, and
/// none of the ones written for it — and then panicking inside the path to tell
/// "never reached" apart from "reached and overridden".
#[test]
fn an_ordering_that_cannot_be_ranked_on_views_is_kept_off_the_view_path() {
    assert!(ordering_travels_on_views("line"));
    assert!(ordering_travels_on_views("param_count"));
    assert!(
        ordering_travels_on_views("has_doc"),
        "no column of a segment need hold has_doc: the view ranks such a row by \
         its absence and so does the row it would build. Agreeing on nothing is \
         still agreeing, and rejecting this case is what would leave the whole \
         path dead on a real corpus, where most segments carry no column for \
         any given enrichment field"
    );
    assert!(
        ordering_travels_on_views("name"),
        "name is a fixed column of every segment and the built row is filled \
         from it, so ordering by it rides views even where an enrichment column \
         shares the name -- which is the majority of segments in this repository"
    );

    assert!(
        !ordering_travels_on_views("usages"),
        "the usages column is a stale zero the workspace count replaces after \
         materialisation, so the view reports it absent -- not zero -- where the \
         built row reports the real count"
    );
    assert!(
        !ordering_travels_on_views("node_id"),
        "the node handle is derived from the row's ordinal as the row is built, \
         so the view would rank by nothing where the built row ranks by a handle"
    );
    assert!(
        !ordering_travels_on_views("count"),
        "count is assigned by GROUP BY after the page is chosen"
    );

    for field in crate::storage::columnar::segment_reader::VIEW_CANNOT_ANSWER {
        assert!(
            !ordering_travels_on_views(field),
            "{field} is published as a field a view cannot answer, so no \
             ordering by it may ride one"
        );
    }
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
    let (pruned, _shed) = ColumnarStorage::topk_rows_of_segment(
        &ready,
        &clauses,
        ColumnarStorage::topk_trim_for(&clauses),
    )
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
        ColumnarStorage::topk_rows_of_segment(
            &waiting,
            &clauses,
            ColumnarStorage::topk_trim_for(&clauses)
        )
        .is_none(),
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
            ColumnarStorage::topk_rows_of_segment(
                &narrowed,
                &clauses,
                ColumnarStorage::topk_trim_for(&clauses)
            )
            .is_some(),
            fires,
            "LIMIT {k} over {matched} matched rows: this must shed exactly when \
             the running trim would"
        );
    }
}

/// The two row builders must produce the same row.
///
/// `materialize_rows` resolves each column once for a whole batch and serves
/// every scan; `materialize_one_row` builds one row at a time and serves the
/// symbol resolvers (`query/resolve.rs`) and the overlay's name streams
/// (`overlay.rs`). The same query can reach a row through either, so two
/// builders for one row is exactly the shape that drifts — one gains a field
/// the other does not — and the drift shows up as a query answering
/// differently depending on which verb asked, which no test of either builder
/// alone can see.
///
/// This lives here rather than beside the builders because the ranking added
/// in this module made the two paths comparable for the first time, and the
/// fixture it needs already exists here.
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
    for field in crate::filter::ORDER_TIE_BREAKERS {
        assert!(
            ranks_field_like_a_built_row(field),
            "{field} is published as a tie-breaker but a row view cannot rank \
             by it, so the gate would admit a ranking that reads it from the \
             built row only"
        );
    }
    for field in crate::storage::columnar::segment_reader::VIEW_CANNOT_ANSWER {
        assert!(
            !crate::filter::ORDER_TIE_BREAKERS.contains(field),
            "{field} is both a published tie-breaker and a field a view cannot \
             answer, which would leave no ordering able to ride a view at all"
        );
    }

    // The ordering reader asks the same guard the predicate reader asks. A
    // deferred predicate is merely slower; an ordering that ranks every row by
    // an absence the built row does not share cuts the wrong top-K and sheds
    // rows that belonged in the answer, so this is the reader that can least
    // afford to admit `body` or `role`.
    for field in ["body", "role"] {
        assert!(
            !ranks_field_like_a_built_row(field),
            "{field} is written onto the row after its columns are read, so a \
             view ranking by it ranks by an absence the built row may not share"
        );
    }
    for field in ["has_doc", "lines", "naming", "param_count"] {
        assert!(
            ranks_field_like_a_built_row(field),
            "{field} is an ordinary enrichment name — refusing it here would \
             take the route back to almost nothing"
        );
    }
}

/// Every field a row view answers must read the same on the view as on the row
/// that view would build.
///
/// The whole design rests on that sentence, so this checks it against the two
/// readers rather than restating it, on every column each fixture holds and on
/// the struct-backed names besides. The page-level test above would catch a
/// divergence only where it happens to change the ranking, which a field that
/// ties on every fixture row never does — so a divergence could sit in
/// `language` or `path` indefinitely and surface on a corpus instead.
///
/// The list deliberately includes `has_doc`, which no column here holds: a view
/// and a built row both report it absent, and that agreement is what lets an
/// ordering by an enrichment column ride a view at all.
///
/// It runs over the shadowed fixture as well, and that is the case that cost a
/// measurement round. A segment carrying an enrichment column named `fql_kind`
/// or `name` used to be refused the view path outright, on the ground that the
/// view reported the field absent while the built row answered from its own
/// struct. The view now reads the same fixed column the built row is filled
/// from, and `WHERE name = 42` — the one operator that reaches the shadow on a
/// built row — reaches it on the view too, which is what the numeric half of
/// this loop pins.
#[test]
fn a_view_reads_every_field_as_the_row_it_builds() {
    use crate::filter::ClauseTarget as _;

    let plain = ranked_segment();
    let shadowed = shadowed_kind_segment();
    for (label, (_tmp, seg), source_path) in [
        ("plain", plain, Path::new("src/ranked.rs")),
        ("shadowed", shadowed, Path::new("src/shadowed.rs")),
    ] {
        let all: RoaringBitmap = (0..seg.row_count).collect();
        let built = seg.materialize_rows(&all, Some(source_path));

        // Every column this segment holds, plus every name a built row answers
        // from its own struct, plus one no column holds anywhere.
        let mut fields: Vec<String> = seg
            .enrichment_columns()
            .map(|(name, _)| name.to_owned())
            .collect();
        for name in ["name", "node_kind", "fql_kind", "language", "path", "line"] {
            if !fields.iter().any(|f| f == name) {
                fields.push(name.to_owned());
            }
        }
        fields.push("has_doc".to_owned());

        for field in &fields {
            assert!(
                ranks_field_like_a_built_row(field),
                "{label}: {field} is not admitted, so this loop would be \
                 checking a field the gate never lets through"
            );
            for (row, built_row) in all.iter().zip(&built) {
                let view = RowView::of(&seg, Some(source_path), row);
                assert_eq!(
                    view.field_str(field),
                    built_row.field_str(field),
                    "{label} row {row}: {field} reads differently as a string \
                     on the view and on the row it builds"
                );
                assert_eq!(
                    view.field_num(field),
                    built_row.field_num(field),
                    "{label} row {row}: {field} reads differently as a number \
                     on the view and on the row it builds"
                );
            }
        }

        // And the counterpart: a field the gate refuses is refused because the
        // two readers really do disagree, not out of caution. `usages` is the
        // one that matters most, since ORDER BY usages is a documented recipe.
        assert!(!ranks_field_like_a_built_row("usages"));
        let view = RowView::of(&seg, Some(source_path), 0);
        assert_eq!(view.field_num("usages"), None);
        assert_eq!(built[0].field_num("usages"), Some(0));
    }
}

/// The whole-query gate opens for the shapes the memory win depends on, and
/// closes for each shape that would move an answer.
///
/// A golden case cannot see this. Both routes answer the same query with the
/// same rows by construction, which is the point — so a regression that quietly
/// stops taking the view route leaves every golden green while every scan goes
/// back to building millions of rows to deliver twenty. What found exactly that
/// during development was a panic inside the route, on a real corpus; this is
/// the same question asked where the suite can keep asking it.
/// The one condition the view bound may drop that the running trim may not.
///
/// Both are asked of the same clauses here, because the pair is the whole
/// argument: the view route carries `limit + offset` views, so the rows an
/// `OFFSET` pages to are inside its window; the trim retains `topk_keep(limit)`
/// and nothing more, so those same rows are the ones it has been discarding.
/// Relaxing the trim to match the bound — the obvious tidy-up on reading the
/// two side by side — returns a short page and says nothing about it.
#[test]
fn the_trim_declines_an_offset_where_the_view_bound_carries_it() {
    let offset_page = Clauses {
        limit: Some(20),
        offset: Some(100),
        ..Default::default()
    };
    assert_eq!(
        ColumnarStorage::view_page_bound_for(&offset_page),
        Some(120),
        "the view route carries the offset as rows it must hold"
    );
    assert_eq!(
        ColumnarStorage::topk_trim_for(&offset_page),
        None,
        "and the trim declines the same clauses, because it keeps only the k \
         best it has seen — the rows an OFFSET wants are the ones it dropped"
    );
}

#[test]
fn the_view_route_opens_for_a_plain_bounded_scan_and_closes_where_it_must() {
    let (_tmp, seg) = ranked_segment();
    let paged = |limit: Option<usize>| Clauses {
        limit,
        ..Default::default()
    };

    assert_eq!(
        ColumnarStorage::view_page_bound_for(&paged(Some(20))),
        Some(20),
        "a bare LIMIT is the shape a default FIND arrives as, and it is the \
         whole reason this route exists"
    );
    assert_eq!(
        ColumnarStorage::view_page_bound_for(&Clauses {
            limit: Some(20),
            offset: Some(100),
            ..Default::default()
        }),
        Some(120),
        "OFFSET rows are still rows the page needs: the skip runs downstream, \
         after uncommitted rows have been merged in"
    );
    assert_eq!(
        ColumnarStorage::view_page_bound_for(&paged(Some(5_000))),
        Some(5_000),
        "the bound is what the caller asked for and not the running trim's \
         threshold, so a page above TOPK_THRESHOLD rides views where the trim \
         over built rows will not arm"
    );

    assert_eq!(
        ColumnarStorage::view_page_bound_for(&paged(None)),
        None,
        "with no LIMIT there is no page to cut to"
    );
    assert_eq!(
        ColumnarStorage::view_page_bound_for(&Clauses {
            limit: Some(20),
            group_by: Some(GroupBy::Field("fql_kind".to_owned())),
            ..Default::default()
        }),
        None,
        "GROUP BY assigns a count no view can carry"
    );
    assert_eq!(
        ColumnarStorage::view_page_bound_for(&Clauses {
            limit: Some(20),
            having_predicates: vec![predicate(
                "lines",
                CompareOp::Gt,
                PredicateValue::Number(10)
            )],
            ..Default::default()
        }),
        None,
        "HAVING runs after the page is cut, so anything shed before it might \
         have qualified"
    );

    // The segment half. A predicate a row view answers leaves the route open;
    // one that has to wait for a built row closes it, and closing it is what
    // keeps a page from being cut over rows the query excludes.
    assert!(segment_rows_can_travel_as_views(
        &seg,
        &[predicate(
            "param_count",
            CompareOp::Gt,
            PredicateValue::Number(4)
        )]
    ));
    assert!(segment_rows_can_travel_as_views(
        &seg,
        &[predicate("line", CompareOp::Gte, PredicateValue::Number(1))]
    ));
    assert!(
        !segment_rows_can_travel_as_views(
            &seg,
            &[predicate(
                "usages",
                CompareOp::Gt,
                PredicateValue::Number(0)
            )]
        ),
        "usages is stamped after materialisation, so no view can test it"
    );
    assert!(
        !segment_rows_can_travel_as_views(
            &seg,
            &[Predicate {
                field: "name".to_owned(),
                op: CompareOp::Matches,
                value: crate::ir::PredicateValue::String("^a".to_owned()),
            }]
        ),
        "a regex is compiled once for a batch of built rows and would be \
         recompiled per row here, so it waits whatever field it names"
    );
}

/// A field NO column of the segment holds leaves the view route open, and this
/// is the case that used to close it.
///
/// The row this segment would build carries `has_doc` in neither its struct nor
/// its enrichment map, so the view and the row agree on `None` and the
/// predicate is decided from the columns. For a whole query that means one
/// segment lacking an enrichment column no longer takes the page off views for
/// every segment that has it — which is the whole of what this change buys, so
/// this is the case that goes red if it stops happening.
#[test]
fn an_absent_column_leaves_the_view_route_open_but_a_regex_still_waits() {
    let (_tmp, seg) = ranked_segment();

    assert!(segment_rows_can_travel_as_views(
        &seg,
        &[predicate(
            "has_doc",
            CompareOp::Eq,
            PredicateValue::String("true".to_owned())
        )]
    ));
    assert!(
        segment_rows_can_travel_as_views(
            &seg,
            &[predicate(
                "has_doc",
                CompareOp::NotEq,
                PredicateValue::String("true".to_owned())
            )]
        ),
        "a negative operator is answered here too, and answers false — the \
         same thing the built row's filter concludes from the same absence"
    );
    assert!(
        !segment_rows_can_travel_as_views(
            &seg,
            &[Predicate {
                field: "has_doc".to_owned(),
                op: CompareOp::NotMatches,
                value: crate::ir::PredicateValue::String("^t".to_owned()),
            }]
        ),
        "NOT MATCHES on an absent field is the one shape where the batch \
         filter and a per-row evaluation disagree, so it must keep waiting"
    );
}

/// A segment holding the same row more than once, with the duplicates sorting
/// ahead of the distinct ones under `ORDER BY name ASC`.
///
/// Six rows agree on every field of the Stage 4 key and are therefore one
/// answer row. A seventh shares their name and line but not their kind, so it
/// is a second one — a key that forgot `fql_kind` would swallow it. Six more
/// follow on distinct lines. Thirteen rows, eight answers.
fn duplicate_heavy_segment() -> (tempfile::TempDir, SegmentReader) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("dups.fqsf");
    let content_id = [0x7C_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    for _ in 0..6 {
        let row = b.emit_row(SymbolRow {
            name: "aaa",
            fql_kind: "function",
            language: "rust",
            line: 1,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        b.set_field(row, "param_count", "0");
    }
    let odd = b.emit_row(SymbolRow {
        name: "aaa",
        fql_kind: "struct",
        language: "rust",
        line: 1,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    b.set_field(odd, "param_count", "0");
    for i in 0..6_u32 {
        let row = b.emit_row(SymbolRow {
            name: "bbb",
            fql_kind: "function",
            language: "rust",
            line: 10 + i,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        b.set_field(row, "param_count", "0");
    }
    b.flush(&path).expect("flush");
    let reader = SegmentReader::open(&path).expect("open");
    (tmp, reader)
}

/// A segment whose enrichment column is named after a struct-backed field.
///
/// The column is called `fql_kind` and carries a value the fixed kind column
/// does not, so every reader that claims to resolve the name as the built row
/// resolves it can be caught here: the fixed column under a string operator,
/// this column under a numeric one. `fql_kind` is also a published tie-breaker,
/// so the same fixture answers whether ranking and keying survive the collision.
fn shadowed_kind_segment() -> (tempfile::TempDir, SegmentReader) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("shadowed.fqsf");
    let content_id = [0x3D_u8; 20];
    let mut b = SegmentBuilder::new("test", &content_id);
    for i in 0..12_u32 {
        let row = b.emit_row(SymbolRow {
            name: "aaa",
            fql_kind: "function",
            language: "rust",
            line: 1 + i,
            byte_start: 0,
            byte_end: 10,
            usages_count: 0,
        });
        b.set_field(row, "fql_kind", "shadow");
    }
    b.flush(&path).expect("flush");
    let reader = SegmentReader::open(&path).expect("open");
    (tmp, reader)
}

/// The bounded choice collapses before it chooses.
///
/// Six of these rows are one row and they sort to the front, so choosing first
/// would fill the retained window with them and hand back a page holding two
/// rows where four were retained — having already discarded distinct rows that
/// belonged in the answer. This is the unit-level form of the failure
/// `crates/forgeql-core/tests/topk_trim_before_dedupe.rs` reproduces end to end.
#[test]
fn duplicates_collapse_before_the_bounded_choice_sheds_anything() {
    let (_tmp, seg) = duplicate_heavy_segment();
    let source_path = Path::new("src/dups.rs");
    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "name".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(2),
        ..Default::default()
    };
    let narrowed = NarrowedSegment {
        seg: &seg,
        source_path,
        rows: (0..seg.row_count).collect(),
        late: Vec::new(),
    };

    let (kept, shed) = ColumnarStorage::topk_rows_of_segment(
        &narrowed,
        &clauses,
        ColumnarStorage::topk_trim_for(&clauses),
    )
    .expect("thirteen rows is past K * TOPK_OVER_FETCH for K = 2");

    let mut built = seg.materialize_rows(&kept, Some(source_path));
    let retained = built.len();
    assert_eq!(
        retained,
        topk_keep(2),
        "the retained size is the trim's own, and it is counted in answer rows"
    );
    dedupe_symbol_matches(&mut built);
    assert_eq!(
        built.len(),
        retained,
        "every row kept must already be distinct — collapsing them again must \
         not shorten the page"
    );
    assert_eq!(
        shed,
        8 - topk_keep(2),
        "thirteen rows are eight answers, so what is shed is counted in \
         answers too: a `total` built from this must not report the thirteen \
         nor the four"
    );
}

/// A segment that shadows a tie-break-and-key field stays ON the view path.
///
/// This case used to assert the opposite, and reversing it is the substance of
/// the change. An enrichment column named `fql_kind` was taken to withhold the
/// field from a row view, so such a segment was refused both the ranking and
/// the collapse and built every row it matched. It withholds nothing: a built
/// row answers `fql_kind` from its own struct field, filled from the fixed kind
/// column, and never from its enrichment map — so a view reading that same
/// column reads the same value. Only `WHERE fql_kind = 42`, which reaches the
/// map on a built row, reaches the shadow, and it reaches it on both readers.
///
/// The cost of the old reading was not theoretical, and its cause was not one
/// enricher: `extract_fields` writes every tree-sitter grammar field of every
/// emitted node as an enrichment column, and `name` is a grammar field on
/// essentially every definition node, so 308 of the 411 segments of this
/// repository's index carry one and every one of them was refused.
#[test]
fn a_segment_that_shadows_a_key_field_still_travels_as_views() {
    use crate::filter::ClauseTarget as _;

    let (_tmp, seg) = shadowed_kind_segment();
    let source_path = Path::new("src/shadowed.rs");
    let rows: RoaringBitmap = (0..seg.row_count).collect();

    assert!(
        seg.answers_field("fql_kind", true, Accessor::Str),
        "an enrichment column named fql_kind shadows the struct-backed field, \
         which is the shape this case is about — and a string predicate on it \
         is still answered from the fixed kind column, because that is the \
         column the built row answers from"
    );
    // And the level that decides the route, which the assert above cannot see.
    // `answers_field` is the resolver; `split_seg_predicates` is what actually
    // puts a predicate on the early side or the late one, and a shadow guard
    // reinstated there would close this route again with every other case in
    // the suite still green — including both golden cases, which check answers
    // and say in their own descriptions that they cannot tell the routes apart.
    let kind_eq = predicate(
        "fql_kind",
        CompareOp::Eq,
        PredicateValue::String("function".to_owned()),
    );
    let (early, late) = split_seg_predicates(&seg, std::slice::from_ref(&kind_eq), true);
    assert_eq!(
        (early.len(), late.len()),
        (1, 0),
        "a predicate on the shadowed name must be answered from the columns, \
         not handed to the filter that runs after the rows are built"
    );
    assert!(
        ordering_travels_on_views("line"),
        "fql_kind is a published tie-breaker, and shadowing it no longer stops \
         a view from ranking by it"
    );

    let built = seg.materialize_rows(&rows, Some(source_path));
    for (row, built_row) in rows.iter().zip(&built) {
        let view = RowView::of(&seg, Some(source_path), row);
        assert_eq!(
            view.collapse_key(),
            (
                built_row.field_str("name").unwrap_or(""),
                built_row.field_str("fql_kind"),
                u32::try_from(built_row.field_num("line").unwrap_or(0)).unwrap_or(0),
            ),
            "row {row}: the view keys this row differently from the row it builds"
        );
    }

    let views: Vec<RowView<'_>> = rows
        .iter()
        .map(|row| RowView::of(&seg, Some(source_path), row))
        .collect();
    let mut by_built = built;
    dedupe_symbol_matches(&mut by_built);
    assert_eq!(
        dedupe_views_of_segment(views).len(),
        by_built.len(),
        "the collapse over views and the collapse over built rows must agree on \
         how many distinct rows this segment holds"
    );

    let clauses = Clauses {
        order_by: Some(OrderBy {
            field: "line".to_owned(),
            direction: SortDirection::Asc,
        }),
        limit: Some(2),
        ..Default::default()
    };
    let narrowed = NarrowedSegment {
        seg: &seg,
        source_path,
        rows,
        late: Vec::new(),
    };
    let (chosen, shed) = ColumnarStorage::topk_rows_of_segment(
        &narrowed,
        &clauses,
        ColumnarStorage::topk_trim_for(&clauses),
    )
    .expect(
        "this segment used to be refused the view path outright for shadowing \
         fql_kind, and building all twelve of its rows to deliver two is what \
         that cost",
    );
    assert_eq!(
        chosen.iter().collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "ORDER BY line ASC LIMIT 2 retains topk_keep(2) = 4 rows, and on this \
         fixture those are lines 1 to 4"
    );
    assert_eq!(
        shed, 8,
        "the other eight matched and were distinct, so they belong to the total"
    );
}

/// The key read from the columns and the key read from the built row put the
/// same rows together.
///
/// This is the invariant the whole collapse rests on, and it is checked as a
/// partition rather than field by field on purpose: the two readings do not
/// have to SPELL an absent value the same way — a row view reports an empty
/// `fql_kind` as absent where the built row carries `None`, and line zero the
/// same way — they only have to agree on which rows are the same row.
#[test]
fn the_column_key_and_the_built_row_key_agree_on_every_pair() {
    let (_tmp, seg) = duplicate_heavy_segment();
    let source_path = Path::new("src/dups.rs");
    let all: RoaringBitmap = (0..seg.row_count).collect();
    let built = seg.materialize_rows(&all, Some(source_path));
    assert_eq!(
        built.len(),
        seg.row_count as usize,
        "materialize_rows must answer one row per selected row id, in order, \
         or the pairing below compares the wrong rows"
    );

    for a in 0..seg.row_count {
        for b in 0..seg.row_count {
            let left = built.get(a as usize).expect("row a is inside the batch");
            let right = built.get(b as usize).expect("row b is inside the batch");
            let by_built_row = left.name == right.name
                && left.fql_kind == right.fql_kind
                && left.path == right.path
                && left.line == right.line;
            assert_eq!(
                RowView::of(&seg, Some(source_path), a).collapse_key()
                    == RowView::of(&seg, Some(source_path), b).collapse_key(),
                by_built_row,
                "rows {a} and {b} are the same row on one reading and not on \
                 the other"
            );
        }
    }
}

/// `GROUP BY <field>` with nothing else written.
fn bare_group_by(field: &str) -> Clauses {
    Clauses {
        group_by: Some(GroupBy::Field(field.to_owned())),
        ..Clauses::default()
    }
}

/// The clause shape the count path accepts is a posted enrichment field and
/// nothing else. `fql_kind` and the path have count paths of their own; a
/// column-only enrichment field such as `param_count` has no `field=value`
/// bitmap to count, and a field admitted here that the key table cannot answer
/// would be answered as one empty group holding every row.
///
/// The gate canonicalises the written field before any of this, as every other
/// eligibility test here does. That is not asserted, and deliberately: no entry
/// of `POSTING_ENRICHMENT_FIELDS` carries an alias in `field_tiers` today, so a
/// case written to prove it would compare `canonical("naming")` against
/// `"naming"` and pass with the call deleted. Give one of these fields an alias
/// and the case becomes writable — and needed, because the value is keyed into
/// the group row's field map under the canonical name and read back out under
/// it by the renderer.
#[test]
fn only_a_posted_enrichment_field_arms_the_count_path() {
    assert_eq!(
        group_by_enrichment_fast_path_field(&bare_group_by("naming"), true),
        Some("naming")
    );
    for field in [
        "param_count",
        "fql_kind",
        "kind",
        "file",
        "language",
        "name",
    ] {
        assert_eq!(
            group_by_enrichment_fast_path_field(&bare_group_by(field), true),
            None,
            "GROUP BY {field}"
        );
    }
    assert_eq!(
        group_by_enrichment_fast_path_field(&Clauses::default(), true),
        None,
        "no GROUP BY at all"
    );
}

/// The two shapes whose rows the key table does not describe: a session holding
/// uncommitted rows, which are in no segment's postings, and a `WHERE`, which
/// selects a subset of rows the stored cardinalities cannot be narrowed to.
#[test]
fn uncommitted_rows_or_a_where_hand_the_group_back() {
    assert_eq!(
        group_by_enrichment_fast_path_field(&bare_group_by("naming"), false),
        None,
        "a dirty overlay"
    );

    let filtered = Clauses {
        where_predicates: vec![Predicate {
            field: "fql_kind".to_owned(),
            op: CompareOp::Eq,
            value: PredicateValue::String("function".to_owned()),
        }],
        ..bare_group_by("naming")
    };
    assert_eq!(
        group_by_enrichment_fast_path_field(&filtered, true),
        None,
        "a WHERE predicate"
    );
}

/// A group row from this path carries the grouped value and its count. Shaping
/// on either is the same question it was over the scan's representative rows;
/// shaping on anything else is a different one, and is handed back rather than
/// answered from a row that never held the field.
#[test]
fn count_and_the_grouped_field_are_the_only_two_the_group_row_answers() {
    let order_by = |field: &str| Clauses {
        order_by: Some(OrderBy {
            field: field.to_owned(),
            direction: SortDirection::Desc,
        }),
        ..bare_group_by("naming")
    };
    assert_eq!(
        group_by_enrichment_fast_path_field(&order_by("count"), true),
        Some("naming")
    );
    assert_eq!(
        group_by_enrichment_fast_path_field(&order_by("naming"), true),
        Some("naming")
    );
    for field in ["name", "line", "usages", "path"] {
        assert_eq!(
            group_by_enrichment_fast_path_field(&order_by(field), true),
            None,
            "ORDER BY {field}"
        );
    }

    let having = |field: &str| Clauses {
        having_predicates: vec![Predicate {
            field: field.to_owned(),
            op: CompareOp::Gte,
            value: PredicateValue::Number(2),
        }],
        ..bare_group_by("naming")
    };
    assert_eq!(
        group_by_enrichment_fast_path_field(&having("count"), true),
        Some("naming")
    );
    assert_eq!(
        group_by_enrichment_fast_path_field(&having("lines"), true),
        None,
        "HAVING on a field the group row does not carry"
    );
}

/// The two predicates a counted `GROUP BY file` may narrow by are `fql_kind =`
/// and `name =`, each with a string value.
///
/// Every arm here is load-bearing. `prefilter_global` SKIPS a predicate no tier
/// serves, and [`ColumnarStorage::fast_group_by_file`] clears the residual
/// `WHERE` before delivering, so a predicate admitted here and unserved there is
/// counted as the whole segment. All three `name` spellings were admitted for
/// years and all three counted high; `open_defects.json` carries the numbers.
/// The equality is admitted again now that `step6_build_name_fst` intersects
/// the name postings with each segment's canonical rows — the patterns are not,
/// and cannot be, since the trigram tier over-generates by construction.
#[test]
fn only_an_exact_kind_or_name_predicate_narrows_a_counted_file_group() {
    let with = |pred: crate::ir::Predicate| Clauses {
        where_predicates: vec![pred],
        ..bare_group_by("file")
    };
    assert!(
        group_by_file_fast_path_eligible(&bare_group_by("file"), true),
        "no WHERE at all counts from dedup_row_count"
    );
    assert!(
        group_by_file_fast_path_eligible(
            &with(predicate(
                "fql_kind",
                CompareOp::Eq,
                PredicateValue::String("function".to_owned())
            )),
            true
        ),
        "the canonical kind postings are exact"
    );
    assert!(
        group_by_file_fast_path_eligible(
            &with(predicate(
                "name",
                CompareOp::Eq,
                PredicateValue::String("0".to_owned())
            )),
            true
        ),
        "the name postings are canonical-intersected too, so a name equality counts"
    );
    for (label, pred) in [
        (
            "name LIKE",
            predicate(
                "name",
                CompareOp::Like,
                PredicateValue::String("%ab%".to_owned()),
            ),
        ),
        (
            "name MATCHES",
            predicate(
                "name",
                CompareOp::Matches,
                PredicateValue::String("^a".to_owned()),
            ),
        ),
        (
            "fql_kind = <number>, which prefilter_global has no arm for",
            predicate("fql_kind", CompareOp::Eq, PredicateValue::Number(2)),
        ),
        (
            "fql_kind LIKE",
            predicate(
                "fql_kind",
                CompareOp::Like,
                PredicateValue::String("fun%".to_owned()),
            ),
        ),
        (
            "an enrichment predicate",
            predicate("lines", CompareOp::Gte, PredicateValue::Number(2)),
        ),
    ] {
        assert!(
            !group_by_file_fast_path_eligible(&with(pred), true),
            "{label} must go to the scan"
        );
    }
}

/// A counted group row carries the grouped value and its count and nothing else,
/// so those are the only two an aggregate clause may name — on the file grouping
/// as on the enrichment one.
///
/// Without this test the gate can lose the check and no golden would see it: the
/// answer to `HAVING lines >= 2` would go back to being an empty set, which
/// reads as a fact about the corpus rather than as a path declining.
#[test]
fn count_and_the_file_are_the_only_two_the_file_group_row_answers() {
    let order_by = |field: &str| Clauses {
        order_by: Some(OrderBy {
            field: field.to_owned(),
            direction: SortDirection::Desc,
        }),
        ..bare_group_by("file")
    };
    assert!(group_by_file_fast_path_eligible(&order_by("count"), true));
    assert!(group_by_file_fast_path_eligible(&order_by("file"), true));
    assert!(group_by_file_fast_path_eligible(&order_by("path"), true));
    for field in ["name", "line", "usages", "fql_kind", "lines"] {
        assert!(
            !group_by_file_fast_path_eligible(&order_by(field), true),
            "ORDER BY {field}"
        );
    }

    let having = |field: &str| Clauses {
        having_predicates: vec![predicate(field, CompareOp::Gte, PredicateValue::Number(2))],
        ..bare_group_by("file")
    };
    assert!(group_by_file_fast_path_eligible(&having("count"), true));
    assert!(
        !group_by_file_fast_path_eligible(&having("lines"), true),
        "HAVING on a field the group row does not carry"
    );
}

/// The same two on the kind grouping — and `name` is the one that must NOT pass.
///
/// Its group row happens to carry the kind string in the name field, so a
/// `HAVING name = 'function'` would answer something; it would just not be the
/// scan's answer, where the name is the first row's own.
#[test]
fn count_and_the_kind_are_the_only_two_the_kind_group_row_answers() {
    let order_by = |field: &str| Clauses {
        order_by: Some(OrderBy {
            field: field.to_owned(),
            direction: SortDirection::Desc,
        }),
        ..bare_group_by("fql_kind")
    };
    assert!(group_by_kind_fast_path_eligible(&order_by("count"), true));
    assert!(group_by_kind_fast_path_eligible(
        &order_by("fql_kind"),
        true
    ));
    for field in ["name", "line", "usages", "path", "lines"] {
        assert!(
            !group_by_kind_fast_path_eligible(&order_by(field), true),
            "ORDER BY {field}"
        );
    }

    let having = |field: &str| Clauses {
        having_predicates: vec![predicate(field, CompareOp::Gte, PredicateValue::Number(2))],
        ..bare_group_by("fql_kind")
    };
    assert!(group_by_kind_fast_path_eligible(&having("count"), true));
    for field in ["lines", "name"] {
        assert!(
            !group_by_kind_fast_path_eligible(&having(field), true),
            "HAVING {field}"
        );
    }
}

/// No regex predicate is answered before the rows are built — on any field,
/// including one the segment holds a column for.
///
/// This is now a cost rule, and only a cost rule: a pattern is compiled once
/// for a batch of built rows rather than once per row. It was more than that.
/// `apply_where_predicates` and `eval_predicate_on` used to disagree on a
/// `NOT MATCHES` whose value is absent: the batch path computed
/// `is_some_and(is_match)` and kept the row when that was `false == false`,
/// while `eval_predicate_on` computed `is_some_and(|v| !is_match(v))` and
/// dropped it, and nothing reached the disagreement only because this line
/// holds every regex to the batch path. Both now fail a negation on a missing
/// value, so what this line still buys is the compile.
///
/// The case is kept because the cost is real and because the shape it covers is
/// the harder one to keep on the slow side: a field the segment *does* carry a
/// column for still waits, even though an unset slot in a stored column reads
/// as absent, so a relaxation aimed at "fields the segment answers" has to be
/// argued rather than assumed.
#[test]
fn a_regex_waits_for_a_built_row_even_on_a_column_the_segment_holds() {
    let (_tmp, reader) = one_row_segment();

    assert!(
        reader.answers_field("param_count", true, Accessor::Str),
        "the fixture must carry this column, or the test is asking nothing"
    );

    for op in [CompareOp::Matches, CompareOp::NotMatches] {
        assert!(
            predicate_waits_for_a_built_row(
                &reader,
                &predicate("param_count", op, PredicateValue::String("^2$".to_owned())),
                true
            ),
            "{op:?} on a column this segment holds must still wait: the two \
             readers disagree on an absent value, and an unset slot in a held \
             column is absent"
        );
    }
}

/// A `WHERE` on `name` where the segment also carries an enrichment column
/// called `name` is routed EARLY — asserted where the routing decision is made.
///
/// `a_segment_that_shadows_a_key_field_still_travels_as_views` already covers a
/// shadowed `fql_kind`. `name` is the case that motivated the slice and the one
/// no fixture held: `extract_fields` writes every tree-sitter grammar field as
/// an enrichment column and `name` is a grammar field on essentially every
/// definition node, so 308 of 411 segments of this repository carry one — the
/// refusal this slice removed took three quarters of every scan off the cheap
/// route.
///
/// It is asserted at the router and not only at the reader, because the refusal
/// lived in the router: reinstating it there for `name` leaves all 995 library
/// tests green, since no reader-level case can see a route close.
#[test]
fn a_shadowed_name_is_routed_early_where_the_decision_is_made() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("shadowed_name.fqsf");
    let mut b = SegmentBuilder::new("test", &[0x5E_u8; 20]);
    let row = b.emit_row(SymbolRow {
        name: "alpha",
        fql_kind: "function",
        language: "rust",
        line: 3,
        byte_start: 0,
        byte_end: 10,
        usages_count: 0,
    });
    // What `extract_fields` writes for a grammar field called `name`.
    b.set_field(row, "name", "the_identifier_child");
    b.flush(&path).expect("flush");
    let seg = SegmentReader::open(&path).expect("open");

    assert!(
        seg.enrichment_columns().any(|(n, _)| n == "name"),
        "the fixture must carry the shadowing column, or this asserts nothing"
    );

    for value in [
        PredicateValue::String("alpha".to_owned()),
        PredicateValue::Number(42),
    ] {
        let pred = predicate("name", CompareOp::Eq, value);
        assert!(
            !predicate_waits_for_a_built_row(&seg, &pred, true),
            "a shadowed `name` must not be deferred: deferring it is the refusal \
             this slice removed, and no reader-level case can see it come back"
        );
        let (early, late) = split_seg_predicates(&seg, std::slice::from_ref(&pred), true);
        assert_eq!(
            early.len(),
            1,
            "and it lands in the early half of the split"
        );
        assert!(late.is_empty());
    }
}
