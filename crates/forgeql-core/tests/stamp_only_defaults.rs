//! The stamp-only boolean defaults: `has_todo`, `has_escape`, `has_shadow` and
//! `is_recursive` answering the value nothing stores.
//!
//! Each is written only when it holds. Read literally that leaves the other
//! value unqueryable — nothing stores it, so nothing selects it, and the empty
//! page reads as a claim about the corpus. The index already implies the
//! answer, so it is computed: the rows the enricher examined, minus the rows it
//! wrote.
//!
//! Which rows those are takes TWO facts. The applicable KINDS, and the
//! applicable LANGUAGES — an enricher gates on a language capability as well as
//! on the node kind, and a language declaring none of the syntax it needs makes
//! it return before reading a byte. Both are declared once in `field_tiers`,
//! beside the value, and a row outside either one answers neither value.
//!
//! The reason these tests exist at all is that the readers deriving from that
//! declaration have to agree: the workspace bitmap prefilter, the per-row
//! evaluator, and the counted grouping that answers without reading rows at
//! all. A prefilter that proposed a row the evaluator then rejected would hand
//! back a page shorter than the count printed beside it.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::uninlined_format_args,
    unused_results
)]

mod common;

use forgeql_core::field_tiers;

/// Every field the ruling names, so no test in this file can special-case one
/// and leave the others silently empty.
const STAMP_ONLY: [&str; 4] = ["has_todo", "has_escape", "has_shadow", "is_recursive"];

/// The fixture holds four functions and one struct.
const FUNCTIONS_IN_FIXTURE: usize = 4;

/// The Python fixture holds five functions: four written at module level and
/// the nested one inside the shadowing case.
const FUNCTIONS_IN_PY_FIXTURE: usize = 5;

fn session() -> common::TestSession {
    common::legacy_session(&["stamp_defaults.cpp"])
}

fn total_for(s: &mut common::TestSession, fql: &str) -> usize {
    let r = s.exec(fql);
    common::as_query(&r).total
}

/// The whole claim, on every field the ruling names: the two values partition
/// the applicable rows, and nothing is lost between them.
///
/// Stated as a SUM rather than as four frozen numbers on purpose. A count that
/// is merely non-zero would pass while the arithmetic was wrong by any amount;
/// `true + false == the applicable rows` cannot.
#[test]
fn the_two_values_partition_the_applicable_rows() {
    let mut s = session();
    let functions = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cpp' WHERE fql_kind = 'function' LIMIT 50",
    );
    assert_eq!(functions, FUNCTIONS_IN_FIXTURE);

    for field in STAMP_ONLY {
        let yes = total_for(
            &mut s,
            &format!("FIND symbols IN 'stamp_defaults.cpp' WHERE {field} = 'true' LIMIT 50"),
        );
        let no = total_for(
            &mut s,
            &format!("FIND symbols IN 'stamp_defaults.cpp' WHERE {field} = 'false' LIMIT 50"),
        );
        assert_eq!(
            yes + no,
            functions,
            "{field}: 'true' answered {yes} and 'false' answered {no}, which do not \
             partition the {functions} functions the enricher examined",
        );
    }
}

/// The rows the fixture was built to distinguish, named rather than counted.
#[test]
fn the_stamped_row_is_the_one_missing_from_the_default() {
    let mut s = session();
    for (field, stamped) in [
        ("has_todo", "functionWithTodo"),
        ("is_recursive", "recursiveFunction"),
        ("has_shadow", "shadowingFunction"),
    ] {
        let r = s.exec(&format!(
            "FIND symbols IN 'stamp_defaults.cpp' WHERE {field} = 'false' ORDER BY name ASC LIMIT 50"
        ));
        let names: Vec<&str> = common::as_query(&r)
            .results
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        assert!(
            names.contains(&"plainFunction"),
            "{field} = 'false' must answer the function that carries nothing, got {names:?}",
        );
        assert!(
            !names.contains(&stamped),
            "{field} = 'false' must NOT answer {stamped}, which carries 'true', got {names:?}",
        );
    }
}

/// The exclusion that makes the default a fact rather than a fabrication: a row
/// of a kind no function enricher examines answers NEITHER value.
///
/// This is the half that cannot be got from the field alone. A struct row
/// carries no `has_todo` for the same reason a TODO-less function does — the
/// column is simply absent — and only the declared applicable kinds separate
/// "examined and found nothing" from "never looked at".
#[test]
fn a_kind_the_enricher_never_examines_answers_neither_value() {
    let mut s = session();
    for field in STAMP_ONLY {
        for value in ["true", "false"] {
            let r = s.exec(&format!(
                "FIND symbols IN 'stamp_defaults.cpp' WHERE fql_kind = 'struct' \
                 WHERE {field} = '{value}' LIMIT 10"
            ));
            let q = common::as_query(&r);
            assert_eq!(
                q.total, 0,
                "a struct row answered {field} = '{value}'; the default speaks only for \
                 the kinds the enricher examines",
            );
        }
    }
}

/// The boundary, stated as a test rather than only in prose: the OTHER
/// stamp-only booleans are deliberately untouched and still answer nothing.
///
/// `has_fallthrough` is written by the same kind of enricher and would be one
/// row of declaration away, and that is exactly why this is pinned — the
/// difference between "not yet declared" and "declared and broken" has to be
/// visible. When one of these gains a declaration this test goes red, which is
/// the prompt to move the doc sentence naming them with it.
#[test]
fn an_undeclared_stamp_only_boolean_still_answers_nothing() {
    let mut s = session();
    for field in ["has_fallthrough", "is_const", "has_catch_all"] {
        assert!(
            field_tiers::stamp_default(field).is_none(),
            "{field} now declares a default; move the scope sentence in doc/syntax.md \
             and the agent docs, and this test with it",
        );
        let total = total_for(
            &mut s,
            &format!("FIND symbols IN 'stamp_defaults.cpp' WHERE {field} = 'false' LIMIT 10"),
        );
        assert_eq!(total, 0, "{field} = 'false' answered {total} rows");
    }
}

/// The other direction of the applicable-set boundary, on a fixture built for
/// it: a row the enricher EXAMINED whose kind is outside the declared set.
///
/// cmake declares both `function_def` and `macro_def` its function kinds, so
/// every enricher here examines a `macro_def` — and its `kind_map` sends the
/// row to `fql_kind = 'macro'`. Such a row is stamped when the field holds and
/// answers nothing when it does not, exactly as before this table gained
/// defaults, and that asymmetry is the thing to notice rather than to discover
/// later on a corpus.
///
/// `param_count` is the witness. The metrics enricher stamps it on every row
/// its function-kind gate admits and stamps it unconditionally, so a row
/// carrying it was examined by the very gate `has_shadow` uses. Asserting it
/// here is what stops this test reading as "cmake macros have no shadowing":
/// without it, an empty answer would prove nothing.
#[test]
fn an_examined_row_outside_the_declared_kinds_keeps_its_old_answer() {
    let mut s = common::legacy_session(&["stamp_defaults.cmake"]);

    let examined = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cmake' WHERE fql_kind = 'macro' \
         WHERE param_count >= 0 LIMIT 10",
    );
    assert_eq!(
        examined, 1,
        "the fixture's cmake macro must be a row the function-kind gate admitted; \
         if this is 0 the test below proves nothing",
    );

    let defaulted = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cmake' WHERE fql_kind = 'macro' \
         WHERE has_shadow = 'false' LIMIT 10",
    );
    assert_eq!(
        defaulted, 0,
        "a row outside the declared applicable kinds must keep answering nothing. \
         If this now answers, the declaration was widened — move the exclusion \
         sentence in doc/syntax.md, the agent docs, the changelog fragment and \
         StampDefault's own doc with it",
    );

    // Two reasons hold this row out now, and only one of them is the kind: the
    // only language that declares a function kind and maps it elsewhere is
    // cmake, and cmake is outside every one of the four language lists as well.
    // So this case can no longer isolate the kind exclusion on its own. What
    // does isolate it is `a_kind_the_enricher_never_examines_answers_neither_value`
    // over the C++ fixture, where the language IS listed and only the kind
    // holds the struct row out. Both are kept: this one still fails if the
    // kinds are widened to cover `macro`.
}

/// The LANGUAGE half of the applicable set, on the language that made it
/// necessary.
///
/// `EscapeEnricher` gates on `has_address_of` and Python declares none, so it
/// returns before reading anything and no Python function has ever been
/// examined for escaping locals. Those rows are inside the applicable kinds and
/// outside the applicable languages, and they must answer NEITHER value: a
/// `'false'` here is a claim about an analysis that never ran, and it would be
/// made about every Python function in the workspace.
#[test]
fn a_language_whose_enricher_never_ran_answers_neither_value() {
    let mut s = common::legacy_session(&["stamp_defaults.py"]);

    let functions = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.py' WHERE fql_kind = 'function' LIMIT 50",
    );
    assert_eq!(
        functions, FUNCTIONS_IN_PY_FIXTURE,
        "the fixture must hold Python function rows; if this is 0 nothing below \
         proves anything",
    );

    for value in ["true", "false"] {
        let answered = total_for(
            &mut s,
            &format!("FIND symbols IN 'stamp_defaults.py' WHERE has_escape = '{value}' LIMIT 50"),
        );
        assert_eq!(
            answered, 0,
            "has_escape = '{value}' answered {answered} Python rows. Python declares no \
             address-of operator, so EscapeEnricher returns before reading anything and \
             neither value is a fact about these rows",
        );
    }
}

/// The control for the test above: the SAME rows answer the fields whose
/// enrichers do run on Python.
///
/// Without this, an exclusion that swallowed the whole language — or the whole
/// file, or every stamp-only field at once — would pass just as well.
#[test]
fn the_fields_that_do_run_on_that_language_still_partition_it() {
    let mut s = common::legacy_session(&["stamp_defaults.py"]);

    let functions = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.py' WHERE fql_kind = 'function' LIMIT 50",
    );

    for field in ["has_todo", "is_recursive", "has_shadow"] {
        let yes = total_for(
            &mut s,
            &format!("FIND symbols IN 'stamp_defaults.py' WHERE {field} = 'true' LIMIT 50"),
        );
        let no = total_for(
            &mut s,
            &format!("FIND symbols IN 'stamp_defaults.py' WHERE {field} = 'false' LIMIT 50"),
        );
        assert_eq!(
            yes + no,
            functions,
            "{field}: 'true' answered {yes} and 'false' answered {no}, which do not \
             partition the {functions} Python functions the enricher examined",
        );
        assert!(
            yes >= 1,
            "{field}: no Python row carries 'true', so this file cannot show the \
             enricher ran on Python at all and the partition above proves nothing",
        );
    }
}

/// A row inside the applicable KINDS that no enricher here ever examined.
///
/// cmake declares function kinds and its `function_def` maps to
/// `fql_kind = 'function'`, so the kind gate admits it and an enricher on that
/// gate alone — the metrics one, whose `param_count` is the witness below —
/// really did stamp it. Every one of the four still returned before reading
/// anything: cmake declares no comment kind, no call expression and no
/// address-of operator, and its grammar node carries no `body` field for the
/// shadow walk to start on.
///
/// So one row that IS a function row answers neither value on all four fields,
/// which is what no kind-only declaration could express. The other half of the
/// pair is `stamp_defaults.py`, where the same query shape answers on three
/// fields and not on `has_escape`.
#[test]
fn a_language_no_enricher_examines_answers_neither_value_on_every_field() {
    let mut s = common::legacy_session(&["stamp_defaults.cmake"]);

    let functions = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cmake' WHERE fql_kind = 'function' \
         WHERE param_count >= 0 LIMIT 10",
    );
    assert_eq!(
        functions, 1,
        "the fixture's cmake function must be a row the kind gate admits and an \
         enricher on that gate alone did stamp — `param_count` is the witness. If \
         this is 0 the assertions below prove nothing",
    );

    for field in STAMP_ONLY {
        for value in ["true", "false"] {
            let answered = total_for(
                &mut s,
                &format!(
                    "FIND symbols IN 'stamp_defaults.cmake' WHERE fql_kind = 'function' \
                     WHERE {field} = '{value}' LIMIT 10"
                ),
            );
            assert_eq!(
                answered, 0,
                "{field} = '{value}' answered {answered} cmake rows. cmake declares no \
                 comment kind, no call expression and no address-of operator, and its \
                 function_def node carries no `body` field for the shadow walk to start \
                 on — so all four enrichers returned before reading anything and neither \
                 value is a fact about this row",
            );
        }
    }
}

/// A target that carries an `fql_kind` and no enrichment columns must not be
/// handed a fabricated default.
///
/// `resolved_field_str` is generic over `ClauseTarget`, and outline and member
/// rows are exactly that shape — they answer `fql_kind` and carry no enrichment
/// map at all. What keeps the default away from them is the per-verb field gate
/// refusing the name before the resolver is ever reached, which lives in a
/// different file from the resolver. This pins that arrangement from the
/// outside, so moving either half without the other goes red here.
#[test]
fn a_verb_with_no_enrichment_columns_refuses_the_field_rather_than_defaulting() {
    let mut s = session();
    let err = s.err("SHOW outline OF 'stamp_defaults.cpp' WHERE has_todo = 'false'");
    assert!(
        !err.is_empty(),
        "SHOW outline must refuse has_todo rather than resolve a default for rows \
         that carry no enrichment columns; got a result instead of an error",
    );
}

/// Every operator whose published answer this changes, in one place.
///
/// The ruling settles `=`; the rest follow from reading the row through the
/// same resolver, and each of them answered nothing before. They are listed
/// together because a fix that moved only `=` would leave a caller who wrote
/// `!= 'true'` with the old silent zero and no way to tell.
#[test]
fn every_operator_reads_the_default_the_same_way() {
    let mut s = session();
    let scope = "FIND symbols IN 'stamp_defaults.cpp'";
    for (label, predicate) in [
        ("not-equal", "WHERE has_todo != 'true'"),
        ("like", "WHERE has_todo LIKE 'fal%'"),
        ("not-like", "WHERE has_todo NOT LIKE 'tru%'"),
        ("matches", "WHERE has_todo MATCHES '^fal'"),
        ("not-matches", "WHERE has_todo NOT MATCHES '^tru'"),
    ] {
        let total = total_for(&mut s, &format!("{scope} {predicate} LIMIT 50"));
        assert_eq!(
            total,
            FUNCTIONS_IN_FIXTURE - 1,
            "{label} ({predicate}) answered {total}; the three functions carrying no \
             marker resolve to 'false' and the struct row resolves to nothing",
        );
    }
}

/// `GROUP BY` names the group rather than leaving it under the empty key, and
/// `ORDER BY` sorts it where the name says it belongs.
///
/// Both read the same resolver as the predicates, which is the point: a caller
/// who sees a group called `false` and then filters on `has_todo = 'false'`
/// must get the rows that group counted.
#[test]
fn grouping_and_ordering_read_the_default_too() {
    let mut s = session();

    // Grouped over EVERY row, not just the functions, because that is the only
    // shape in which the change is visible on this backend. The scan's grouping
    // keeps the first row of each group and labels it with that row's own name,
    // so the key itself never reaches the result — what moves is which rows
    // share a group. Before, the unstamped functions keyed on the empty string
    // alongside the struct, its field and every comment, and there were two
    // groups; now they key on 'false' and there are three. (The columnar
    // counted route DOES name its groups, and that naming is pinned in
    // stamp_only_defaults.json, which is the other half of this.)
    let r = s.exec("FIND symbols IN 'stamp_defaults.cpp' GROUP BY has_todo LIMIT 10");
    let q = common::as_query(&r);
    let counts: Vec<Option<usize>> = q.results.iter().map(|m| m.count).collect();
    assert_eq!(
        counts.len(),
        3,
        "expected three groups — the stamped function, the three the enricher \
         examined and did not stamp, and everything it never examined — got {counts:?}",
    );
    assert!(
        counts.contains(&Some(FUNCTIONS_IN_FIXTURE - 1)),
        "expected a group of {} for the defaulted functions, got {counts:?}",
        FUNCTIONS_IN_FIXTURE - 1,
    );
    assert!(
        counts.contains(&Some(1)),
        "expected a group of 1 for the stamped function, got {counts:?}",
    );

    // Ordering, over every row for the same reason. With a two-valued field the
    // defaulted rows sort where the valueless ones used to — "" and "false" are
    // both below "true" — so functions alone cannot tell the two readings
    // apart. A row of a kind the enricher never examines can: it still resolves
    // to nothing, "" sorts before "false", and the struct is named to come last
    // by every tie-breaker, so it can only lead on the value it sorted by.
    let r = s.exec("FIND symbols IN 'stamp_defaults.cpp' WHERE fql_kind != 'comment' ORDER BY has_todo ASC LIMIT 20");
    let q = common::as_query(&r);
    let names: Vec<&str> = q.results.iter().map(|m| m.name.as_str()).collect();
    let struct_at = names.iter().position(|n| *n == "zNotAFunction");
    let plain_at = names.iter().position(|n| *n == "plainFunction");
    assert!(
        matches!((struct_at, plain_at), (Some(s), Some(p)) if s < p),
        "ascending by has_todo must put the row that resolves to nothing before \
         the row that resolves to 'false'; a field read one way for ORDER BY and \
         another for WHERE is the disagreement the single declaration prevents. \
         Got {names:?}",
    );
}

/// The mechanism, end to end, in a session that has been written to: adding a
/// marker to a function moves that row from one group to the other.
///
/// A frozen corpus can only ever show the arithmetic holding for one snapshot.
/// This shows it TRACKING — the default is not a number baked in at index time
/// but a statement about which rows the enricher wrote, and it has to follow an
/// edit. Both totals move, in opposite directions, by one.
#[test]
fn adding_a_marker_moves_the_row_between_the_groups() {
    let mut s = session();
    let before_true = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cpp' WHERE has_todo = 'true' LIMIT 50",
    );
    let before_false = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cpp' WHERE has_todo = 'false' LIMIT 50",
    );

    let (handle, rev) = s.file_handle("stamp_defaults.cpp");
    s.exec(&format!(
        "CHANGE NODE '{handle}' IF REV '{rev}' MATCHING 'return x + 1;' \
         WITH 'return x + 1; // TODO: added by the test'"
    ));

    let after_true = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cpp' WHERE has_todo = 'true' LIMIT 50",
    );
    let after_false = total_for(
        &mut s,
        "FIND symbols IN 'stamp_defaults.cpp' WHERE has_todo = 'false' LIMIT 50",
    );

    assert_eq!(after_true, before_true + 1, "the marked row joined 'true'");
    assert_eq!(
        after_false,
        before_false - 1,
        "and left 'false' — a default that did not follow the edit would be a \
         stored number wearing the name of a computed one",
    );

    let r = s.exec(
        "FIND symbols IN 'stamp_defaults.cpp' WHERE has_todo = 'true' \
         ORDER BY name ASC LIMIT 50",
    );
    let names: Vec<&str> = common::as_query(&r)
        .results
        .iter()
        .map(|m| m.name.as_str())
        .collect();
    assert!(
        names.contains(&"plainFunction"),
        "plainFunction carries the new marker and must answer 'true', got {names:?}",
    );
}
