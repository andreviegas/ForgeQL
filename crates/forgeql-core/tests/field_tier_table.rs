//! The field/tier table must agree with the code it describes.
//!
//! The table is a parallel declaration: nothing reads it at query time yet.
//! What makes it worth having before the collapse is this file — every claim
//! it makes is checked here against the const lists, against the builder's
//! own budget functions, and against what a query actually returns. A
//! disagreement is a test failure rather than a wrong answer at corpus scale.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::fmt::Write as _;

use forgeql_core::field_tiers::{
    ALL_OPS, CATCH_ALL_FIELD, Exactness, FIELD_TIERS, FieldTier, Gap, OpClass, Serving, Source,
    Tier, lookup,
};
use forgeql_core::filter::{CORE_WHERE_FIELDS, GROUPABLE_SYMBOL_FIELDS, SORTABLE_SYMBOL_FIELDS};
use forgeql_core::storage::columnar::{
    POSTING_ENRICHMENT_FIELDS, ZONEMAP_NUMERIC_FIELDS, overlay_budget, posting_budget,
};

/// Every name the table answers to, canonical or alias.
fn declared_names() -> BTreeSet<&'static str> {
    FIELD_TIERS
        .iter()
        .flat_map(|t| std::iter::once(t.field).chain(t.aliases.iter().copied()))
        .collect()
}

fn rows_except_catch_all() -> impl Iterator<Item = &'static FieldTier> {
    FIELD_TIERS
        .iter()
        .filter(|t| t.field != CATCH_ALL_FIELD.field)
}

fn eq_serving(row: &FieldTier) -> &'static Serving {
    row.serving
        .iter()
        .find(|s| s.ops.contains(&OpClass::Eq))
        .unwrap_or_else(|| panic!("{} declares no serving for `=`", row.field))
}

// ── Structure: a row cannot quietly forget an operator ──────────────────────

#[test]
fn every_row_accounts_for_every_operator_class() {
    // The Q3 defect in one assertion. Five fields joined the posting index and
    // the compensation was written on the `=` and pattern arms; the four
    // numeric arms read the same keys and were missed by two reviews. A row
    // that names six of seven operator classes is exactly that bug, and here
    // it does not compile past this test.
    for row in FIELD_TIERS {
        let mut seen: Vec<OpClass> = Vec::new();
        for serving in row.serving {
            for op in serving.ops {
                assert!(
                    !seen.contains(op),
                    "{}: operator class {op:?} is declared twice",
                    row.field
                );
                seen.push(*op);
            }
        }
        for op in ALL_OPS {
            assert!(
                seen.contains(op),
                "{}: operator class {op:?} has no declared serving — every class \
                 must be accounted for, scan included",
                row.field
            );
        }
    }
}

#[test]
fn a_tier_that_can_stand_aside_names_what_takes_over() {
    for row in FIELD_TIERS {
        for serving in row.serving {
            match serving.tier {
                Tier::Scan | Tier::Refused | Tier::Unserved => assert!(
                    serving.then.is_none(),
                    "{}: {:?} has nothing to fall through to",
                    row.field,
                    serving.tier
                ),
                Tier::StoredColumn
                | Tier::KeyBitmap
                | Tier::ValueUniverse
                | Tier::NumericIndex
                | Tier::ZoneMap => assert!(
                    serving.then.is_some(),
                    "{}: {:?} stands aside for fields it cannot key, so the row \
                     must name what runs instead",
                    row.field,
                    serving.tier
                ),
                Tier::NameFst | Tier::NamePrefix | Tier::Trigram | Tier::KindBitmap => {}
            }
        }
    }
}

// ── Agreement with the four const lists ────────────────────────────────────

#[test]
fn every_posted_field_is_declared_with_the_builder_s_own_budgets() {
    for &field in POSTING_ENRICHMENT_FIELDS {
        let row = lookup(field).unwrap_or_else(|| {
            panic!("{field} is posted by the segment builder but the table does not name it")
        });
        assert_eq!(
            eq_serving(row).tier,
            Tier::KeyBitmap,
            "{field} is posted, so `=` on it reads one key"
        );
        let budget = row
            .budget
            .unwrap_or_else(|| panic!("{field} is posted under a budget the table does not state"));
        assert_eq!(
            budget.per_file,
            posting_budget(field),
            "{field}: the table's per-file budget disagrees with posting_budget"
        );
        assert_eq!(
            budget.per_workspace,
            overlay_budget(field),
            "{field}: the table's per-workspace budget disagrees with overlay_budget"
        );
        assert!(
            row.gaps.contains(&Gap::OverBudgetFile),
            "{field} is keyed from segment postings, so a file over its budget \
             writes none and this gap is real"
        );
    }
}

#[test]
fn only_posted_fields_claim_the_per_file_gap() {
    // The other direction, and the one that bites. A field keyed by the
    // complete row walk has no per-file gap: claiming one would send the
    // query path looking for rows to add back, which for a row-walk field is
    // the whole corpus. That is the trap Q2 fell into and had to be gated
    // out; here the two directions are asserted together.
    for row in rows_except_catch_all() {
        let posted = POSTING_ENRICHMENT_FIELDS.contains(&row.field);
        assert_eq!(
            row.gaps.contains(&Gap::OverBudgetFile),
            posted,
            "{}: the per-file posting gap and POSTING_ENRICHMENT_FIELDS disagree",
            row.field
        );
    }
    assert!(
        !CATCH_ALL_FIELD.gaps.contains(&Gap::OverBudgetFile),
        "the catch-all stands for row-walk-keyed fields, which have no per-file gap"
    );
}

#[test]
fn every_zone_mapped_column_is_declared() {
    let declared: BTreeSet<&str> = FIELD_TIERS.iter().filter_map(|t| t.column).collect();
    for &(col, _) in ZONEMAP_NUMERIC_FIELDS {
        assert!(
            declared.contains(col),
            "{col} carries a zone map but no table row names it as its column — \
             note the zone-map list is keyed by COLUMN, and `usages` is stored \
             as `usages_count`"
        );
    }
}

#[test]
fn every_validated_field_is_declared() {
    let declared = declared_names();
    for &field in CORE_WHERE_FIELDS {
        assert!(
            declared.contains(field),
            "{field} passes WHERE validation but the table does not say what \
             serves it — that gap is where every defect in this campaign lived"
        );
    }
    for &field in SORTABLE_SYMBOL_FIELDS {
        assert!(
            declared.contains(field),
            "{field} passes ORDER BY validation but the table does not name it"
        );
    }
    for &field in GROUPABLE_SYMBOL_FIELDS {
        assert!(
            declared.contains(field),
            "{field} passes GROUP BY validation but the table does not name it"
        );
    }
}

#[test]
fn no_row_is_invented() {
    // The reverse of the above: a table row must correspond to something the
    // engine really has, or the table drifts in the other direction and
    // starts documenting fields that do not exist.
    let zone_map_columns: BTreeSet<&str> = ZONEMAP_NUMERIC_FIELDS.iter().map(|&(c, _)| c).collect();
    for row in rows_except_catch_all() {
        let known = CORE_WHERE_FIELDS.contains(&row.field)
            || SORTABLE_SYMBOL_FIELDS.contains(&row.field)
            || POSTING_ENRICHMENT_FIELDS.contains(&row.field)
            || row.column.is_some_and(|c| zone_map_columns.contains(c));
        assert!(
            known,
            "{} is declared here but is in none of the lists the engine reads",
            row.field
        );
    }
}

// ── Exactness: which tiers are allowed to conclude an absence ──────────────

#[test]
fn only_a_tier_that_reads_every_row_may_call_a_value_absent() {
    // The Q1 defect, as a rule. A tier assembled from keys sees only what was
    // keyed, so an empty result from it is "no candidates", never "no rows" —
    // unless a per-segment proof has separately established the absence. A
    // tier that compares each row against that row's own stored value is a
    // different thing and may conclude.
    for row in FIELD_TIERS {
        for s in row.serving {
            let legal = matches!(
                (s.tier, s.exactness),
                (Tier::Refused | Tier::Unserved, Exactness::NotApplicable)
                    | (
                        Tier::Scan | Tier::StoredColumn | Tier::NameFst | Tier::KindBitmap,
                        Exactness::Exact,
                    )
                    | (
                        Tier::KeyBitmap
                            | Tier::ValueUniverse
                            | Tier::NumericIndex
                            | Tier::ZoneMap
                            | Tier::Trigram
                            | Tier::NamePrefix
                            | Tier::NameFst,
                        Exactness::Superset,
                    )
                    | (Tier::KeyBitmap, Exactness::SupersetProvenAbsent(_))
            );
            assert!(
                legal,
                "{}: {:?} declared {:?} — a tier built from keys cannot be exact, \
                 and one that reads every row does not need a proof",
                row.field, s.tier, s.exactness
            );
            if let Exactness::SupersetProvenAbsent(proof) = s.exactness {
                assert!(
                    !proof.is_empty(),
                    "{}: a key-derived tier may only conclude an absence through a \
                     named proof",
                    row.field
                );
            }
        }
    }
}

#[test]
fn a_refused_or_unserved_field_promises_no_serving() {
    for row in FIELD_TIERS {
        let dead = row
            .serving
            .iter()
            .any(|s| matches!(s.tier, Tier::Refused | Tier::Unserved));
        if dead {
            assert!(
                row.serving
                    .iter()
                    .all(|s| matches!(s.tier, Tier::Refused | Tier::Unserved)),
                "{}: a field cannot be served for some operators and refused for \
                 others without saying which",
                row.field
            );
            assert!(
                row.gaps.is_empty() && row.budget.is_none(),
                "{}: nothing is indexed, so there is no gap and no budget to state",
                row.field
            );
        }
    }
}

// ── Gaps: each one names a fallback, and each one is covered ───────────────

/// The test that proves each declared gap's fallback is reached, or the
/// reason no test can reach it.
///
/// Adding a `Gap` variant to any row fails `every_declared_gap_is_covered`
/// until it is answered for here, which is the point: a gap nobody has tried
/// to reach is indistinguishable from one that is not covered at all.
const GAP_COVERAGE: &[(Gap, &str)] = &[
    (
        Gap::OverBudgetFile,
        "a_field_over_its_budgets_still_answers_completely, and \
     query_set_valued_fields::a_file_over_its_field_budget_still_answers_completely \
     for the wide budget class",
    ),
    (
        Gap::OverBudgetWorkspace,
        "a_field_over_its_budgets_still_answers_completely — \
     at fixture scale a file that exceeds the per-file budget for a branch-indexed \
     field also exhausts the workspace one, so the two are proven together",
    ),
    (
        Gap::EmptyValue,
        "NOT CONSTRUCTIBLE: the overlay skips a value that is the empty string \
     when it writes keys, and no fixture reaches an enricher that stores one — \
     a row either carries a non-empty value for a field or does not carry the \
     field at all. Recorded as unreached rather than proven absent. The \
     adjacent, constructible risk — a pattern answering from the keys instead \
     of from the rows — is covered by \
     a_pattern_answers_the_rows_the_filter_would_accept",
    ),
    (
        Gap::DirtySession,
        "query_set_valued_fields::an_uncommitted_row_is_found_by_a_set_field_query \
     and query_core_field_tier::an_uncommitted_row_is_found_by_a_language_predicate",
    ),
    (
        Gap::ShortColumn,
        "NOT CONSTRUCTIBLE from a well-formed segment: the guard \
     compares the stored column's length against the row count, and a segment the \
     builder wrote always agrees. Reaching it needs a doctored segment, which no \
     public API can produce",
    ),
    (
        Gap::AboveI64,
        "NOT CONSTRUCTIBLE: the values are u64 hashes and nothing \
     chooses which one a guard group receives, so no fixture can place a row above \
     i64::MAX on purpose. Recorded as a limitation with no fallback rather than \
     tested",
    ),
];

#[test]
fn every_declared_gap_is_covered() {
    let covered: BTreeSet<Gap> = GAP_COVERAGE.iter().map(|&(g, _)| g).collect();
    let declared: BTreeSet<Gap> = FIELD_TIERS
        .iter()
        .flat_map(|t| t.gaps.iter().copied())
        .chain(CATCH_ALL_FIELD.gaps.iter().copied())
        .collect();
    for gap in &declared {
        assert!(
            covered.contains(gap),
            "{gap:?} is declared by some field but GAP_COVERAGE does not say which \
             test reaches its fallback, or why none can"
        );
    }
    // Every suite a coverage note is allowed to point at. A note naming a
    // test that has been renamed or deleted is worse than no note: it reads
    // like proof and is not, which is the shape this whole file exists to
    // catch.
    let suites = concat!(
        include_str!("field_tier_table.rs"),
        include_str!("query_set_valued_fields.rs"),
        include_str!("query_core_field_tier.rs"),
    );
    for (gap, note) in GAP_COVERAGE {
        assert!(
            !gap.fallback().is_empty(),
            "{gap:?} names no fallback mechanism"
        );
        if note.contains("NOT CONSTRUCTIBLE") {
            continue;
        }
        // A note may qualify a test by suite (`suite::test_name`); the suite
        // is a module path, not a function, so only the last segment names
        // something a `fn` line can be found for.
        let named: Vec<&str> = note
            .split(|c: char| c.is_whitespace() || ",;()`".contains(c))
            .filter_map(|t| t.rsplit("::").next())
            .filter(|t| t.contains('_') && t.len() > 12)
            .collect();
        assert!(
            !named.is_empty(),
            "{gap:?}: the coverage note must name the test that reaches its \
             fallback, or begin NOT CONSTRUCTIBLE with the reason, got: {note}"
        );
        for test_name in named {
            assert!(
                suites.contains(&format!("fn {test_name}(")),
                "{gap:?}: the note names `{test_name}`, which no suite defines \
                 — it was renamed or deleted and the note still reads like proof"
            );
        }
    }
}

// ── Behaviour: the table's claims, asked of a real workspace ───────────────

mod common;

use forgeql_core::result::{ForgeQLResult, QueryResult};

fn query(t: &mut common::TestSession, fql: &str) -> QueryResult {
    match t.exec(fql) {
        ForgeQLResult::Query(q) => q,
        other => panic!("expected Query from `{fql}`, got {other:?}"),
    }
}

/// Result row names, sorted — the comparable shape of an answer.
fn names(q: &QueryResult) -> Vec<String> {
    let mut v: Vec<String> = q.results.iter().map(|r| r.name.clone()).collect();
    v.sort_unstable();
    v
}

/// One file whose conditional chain carries `branches` distinct
/// `guard_branch` values — the cheapest way to put a real field over a real
/// budget, since the value is the branch's position.
fn guarded_workspace(branches: usize) -> common::TestSession {
    build_workspace(branches, false)
}

/// The same, plus a second file whose one function sits inside no conditional
/// at all — so the workspace holds rows that carry `guard_branch` and rows
/// that do not.
fn guarded_workspace_with_unguarded(branches: usize) -> common::TestSession {
    build_workspace(branches, true)
}

fn build_workspace(branches: usize, plus_unguarded: bool) -> common::TestSession {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut src = String::new();
    for i in 0..branches {
        let directive = if i == 0 { "#if" } else { "#elif" };
        let _ = writeln!(
            src,
            "{directive} defined(FLAG_{i})\nint sym_{i}(void) {{ return {i}; }}"
        );
    }
    src.push_str("#endif\n");
    std::fs::write(dir.path().join("guards.cpp"), src).expect("write fixture");
    if plus_unguarded {
        std::fs::write(
            dir.path().join("plain.cpp"),
            "int unguarded_sym(void) { return 0; }\n",
        )
        .expect("write fixture");
    }
    common::columnar_session_in(dir)
}

/// The `guard_branch` value the row named `name` actually carries, read off an
/// unfiltered scan so the test never assumes how branches are numbered.
fn branch_value_of(t: &mut common::TestSession, name: &str) -> String {
    let all = query(t, "FIND symbols WHERE fql_kind = 'function' LIMIT 500");
    all.results
        .iter()
        .find(|r| r.name == name)
        .and_then(|r| r.fields.get("guard_branch").cloned())
        .unwrap_or_else(|| panic!("{name} carries no guard_branch in the fixture"))
}

#[test]
fn a_field_over_its_budgets_still_answers_completely() {
    // Nine distinct branch values in one file, against a per-file budget of
    // eight: the file writes no postings at all, so none of the field's keys
    // name its rows. Every row in the workspace is such a row here, which is
    // what makes the assertion bite — without the fallback the answer is
    // empty, not merely short.
    let mut t = guarded_workspace(9);
    assert_eq!(
        posting_budget("guard_branch"),
        8,
        "fixture sizing assumption"
    );

    let groups = query(
        &mut t,
        "FIND symbols GROUP BY guard_branch ORDER BY count DESC",
    );
    assert!(
        groups.results.len() > posting_budget("guard_branch"),
        "fixture did not exceed the per-file budget: {} distinct values",
        groups.results.len()
    );

    let wanted = branch_value_of(&mut t, "sym_8");
    let q = query(
        &mut t,
        &format!("FIND symbols WHERE guard_branch = '{wanted}' LIMIT 50"),
    );
    assert!(
        q.results.iter().any(|r| r.name == "sym_8"),
        "an over-budget file's rows were dropped: `guard_branch = '{wanted}'` \
         returned {:?}",
        q.results.iter().map(|r| &r.name).collect::<Vec<_>>()
    );
}

#[test]
fn a_pattern_answers_the_rows_the_filter_would_accept() {
    // Two things at once, because they fail the same way. The fixture is over
    // its per-file budget, so the field has no keys here at all; and one file
    // carries no `guard_branch` on any row. A pattern tier that answered from
    // its keys would return nothing, and one that stopped narrowing
    // altogether would return the unguarded row too. The answer has to be
    // exactly the rows the row-level filter would accept — which is also why
    // the tier must stand aside for a pattern that accepts the empty string:
    // an empty value is keyed nowhere either.
    let mut t = guarded_workspace_with_unguarded(9);

    let scan = query(&mut t, "FIND symbols WHERE fql_kind = 'function' LIMIT 500");
    let mut expected: Vec<&str> = scan
        .results
        .iter()
        .filter(|r| r.fields.contains_key("guard_branch"))
        .map(|r| r.name.as_str())
        .collect();
    expected.sort_unstable();
    assert!(
        !expected.is_empty() && expected.len() < scan.results.len(),
        "fixture must hold both guarded and unguarded rows, got {}/{}",
        expected.len(),
        scan.results.len()
    );

    let patterned = query(
        &mut t,
        "FIND symbols WHERE fql_kind = 'function' WHERE guard_branch MATCHES '.*' LIMIT 500",
    );
    let mut got: Vec<&str> = patterned.results.iter().map(|r| r.name.as_str()).collect();
    got.sort_unstable();
    assert_eq!(
        got, expected,
        "the pattern answer must be the rows the filter accepts, neither the \
         keyed subset nor the whole scan"
    );
}

#[test]
fn refused_fields_error_rather_than_answering_nothing() {
    let mut t = guarded_workspace(2);
    for row in FIELD_TIERS {
        if !row.serving.iter().any(|s| s.tier == Tier::Refused) {
            continue;
        }
        for name in std::iter::once(row.field).chain(row.aliases.iter().copied()) {
            for fql in [
                format!("FIND symbols WHERE {name} = 'x'"),
                format!("FIND symbols ORDER BY {name} LIMIT 5"),
                format!("FIND symbols GROUP BY {name}"),
            ] {
                assert!(
                    t.try_fql(&fql).is_err(),
                    "{name} is declared Refused but `{fql}` answered instead of \
                     erroring"
                );
            }
        }
    }
}

/// The refusal has to hold on every verb that reaches this backend, not only
/// on the one it was written for — and the verbs it does NOT reach have to be
/// named, not left to be discovered.
///
/// `FIND usages` builds occurrence rows that carry no kind at all, and `FIND
/// files` builds file rows that carry none either; both used to run the
/// clause filter over those rows and return a confident empty answer. `FIND
/// callees OF` is an alias for `SHOW callees OF` and answers with a value
/// rather than a Result, so it still accepts the field and matches nothing —
/// asserted here so the boundary moves only deliberately.
#[test]
fn node_kind_is_refused_on_the_four_find_verbs_that_can_refuse() {
    let mut t = guarded_workspace(2);
    for fql in [
        "FIND symbols WHERE node_kind = 'x'",
        "FIND symbols ORDER BY node_kind LIMIT 5",
        "FIND symbols GROUP BY node_kind",
        "FIND globals WHERE node_kind = 'x'",
        "FIND usages OF 'sym_0' WHERE node_kind = 'x'",
        "FIND usages OF 'sym_0' GROUP BY node_kind",
        "FIND files WHERE node_kind = 'x'",
        "FIND files ORDER BY node_kind LIMIT 5",
        "FIND files GROUP BY node_kind",
        "FIND globals ORDER BY node_kind LIMIT 5",
        "FIND globals GROUP BY node_kind",
        "FIND usages OF 'sym_0' ORDER BY node_kind LIMIT 5",
    ] {
        let err = t
            .try_fql(fql)
            .err()
            .unwrap_or_else(|| panic!("`{fql}` answered instead of erroring"));
        let msg = err.to_string();
        assert!(
            msg.contains("node_kind") && msg.contains("fql_kind") || msg.contains("no kind at all"),
            "`{fql}` was refused without saying what to write instead: {msg}"
        );
    }

    // The stated boundary, asserted rather than assumed: this verb routes to
    // SHOW callees, which answers with a value and has nowhere to put an
    // error, so it accepts the field and matches nothing.
    //
    // `is_ok()` alone would not catch the boundary moving: that path turns
    // every failure into an error payload rather than an `Err`, so the answer
    // itself has to be inspected. This fixture's `sym_0` is `return 0;`, so
    // the word "error" cannot appear in a legitimate callees answer for it.
    let answered = match t.try_fql("FIND callees OF 'sym_0' WHERE node_kind = 'x'") {
        Ok(ForgeQLResult::Show(v)) => format!("{v:?}"),
        other => panic!("FIND callees answered unexpectedly: {other:?}"),
    };
    assert!(
        !answered.contains("error"),
        "FIND callees now reports an error for node_kind — the documented \
         boundary moved, and the four docs saying it accepts the field have \
         to move with it: {answered}"
    );
}

#[test]
fn unserved_fields_really_do_match_nothing() {
    // This test documents a defect rather than an invariant: each of these
    // names passes validation, resolves on no symbol row, and so answers a
    // confident zero. It exists so the family cannot grow quietly, and so
    // that the day one of them is fixed — by refusing it, or by resolving it
    // — this fails and the table has to be brought along.
    let mut t = guarded_workspace(3);
    let populated = query(&mut t, "FIND symbols LIMIT 10");
    assert!(!populated.results.is_empty(), "fixture indexed nothing");

    for row in FIELD_TIERS {
        if !row.serving.iter().any(|s| s.tier == Tier::Unserved) {
            continue;
        }
        for name in std::iter::once(row.field).chain(row.aliases.iter().copied()) {
            // `!= 'zzz'` matches every row that can resolve the field at all,
            // so an empty answer is proof the row cannot resolve it.
            let q = query(
                &mut t,
                &format!("FIND symbols WHERE {name} != 'zzz' LIMIT 10"),
            );
            assert!(
                q.results.is_empty(),
                "{name} now resolves on symbol rows — good, but the table still \
                 declares it Unserved"
            );

            // The same names in GROUP BY are refused rather than answered:
            // grouping keys through `field_str` alone, so an unresolvable name
            // there did not return nothing, it returned one empty-named group
            // holding every row. WHERE still answers empty, GROUP BY errors,
            // and the two halves of that split are pinned together.
            let grouped = format!("FIND symbols GROUP BY {name}");
            assert!(
                t.try_fql(&grouped).is_err(),
                "`{grouped}` answered instead of erroring — an unresolvable \
                 grouping key fabricates a single empty-named group"
            );
        }
    }
}

#[test]
fn the_source_of_a_declared_field_is_consistent_with_its_tier() {
    for row in FIELD_TIERS {
        if row.source == Source::Absent {
            assert!(
                row.serving.iter().all(|s| s.tier == Tier::Refused),
                "{}: nothing stores it, so nothing but a refusal is honest",
                row.field
            );
        }
        if row.source == Source::EnrichmentColumn {
            assert!(
                row.budget.is_some(),
                "{}: enrichment keys are bounded, and the bound belongs here",
                row.field
            );
        }
    }
}

/// `kind` is an alias of `fql_kind` on every row type that prints a kind, and
/// an alias that resolves to nothing is the defect this slice exists to
/// remove.
///
/// It is worth pinning because `kind` is the key a `FIND symbols` row is
/// printed under in JSON output: an agent that copies the field name out of
/// the answer it was just given must get rows back, not a confident zero.
#[test]
fn kind_answers_exactly_as_fql_kind_does() {
    let mut t = guarded_workspace(3);

    let by_alias = query(&mut t, "FIND symbols WHERE kind = 'function' LIMIT 100");
    let by_canonical = query(&mut t, "FIND symbols WHERE fql_kind = 'function' LIMIT 100");
    assert!(
        !by_canonical.results.is_empty(),
        "fixture indexed no functions, so this proves nothing"
    );
    assert_eq!(
        names(&by_alias),
        names(&by_canonical),
        "`kind` must answer as `fql_kind` on symbol rows"
    );

    // The same on the two row types that print the column under that name.
    //
    // Compared on the entries the predicate selected, not on the whole
    // response: `SHOW outline` derives each row's `depth` differently
    // depending on whether the filter ran during the tree walk or after it,
    // so `WHERE kind` reports 0 where `WHERE fql_kind` reports the structural
    // depth. That split predates this alias — it is there for every predicate
    // that is not `fql_kind` — and it is a separate defect, not this claim.
    let alias = "SHOW outline OF 'guards.cpp' WHERE kind = 'function'";
    let canonical = "SHOW outline OF 'guards.cpp' WHERE fql_kind = 'function'";
    let mut selected = |fql: &str| {
        let rendered = format!("{:?}", t.try_fql(fql).expect("query"));
        let mut found: Vec<String> = rendered
            .split("name: \"")
            .skip(1)
            .filter_map(|s| s.split('"').next().map(str::to_owned))
            .collect();
        found.sort_unstable();
        found
    };
    let from_alias = selected(alias);
    let from_canonical = selected(canonical);
    assert!(
        !from_alias.is_empty(),
        "the outline fixture selected nothing, so this proves nothing"
    );
    assert_eq!(
        from_alias, from_canonical,
        "`{alias}` and `{canonical}` must select the same entries"
    );
}

/// A `GROUP BY` this release declares groupable must render real groups, not
/// one fabricated group holding everything.
///
/// CSV is the default output format, and the compact renderer keys each row
/// through `group_key`, falling back to the literal `(empty)` when the row
/// could not resolve the field. That is exactly the shape the columnar
/// backend's own refusal text calls out as indistinguishable from an answer —
/// so a field admitted to `GROUPABLE_SYMBOL_FIELDS` has to survive the render
/// too, or the refusal was traded for a fabrication one layer further out.
#[test]
fn newly_groupable_fields_render_real_groups() {
    let mut t = guarded_workspace(3);
    let mut csv = |fql: &str| forgeql_core::compact::to_compact(&t.exec(fql));

    // `kind` is an alias of `fql_kind`: the two spellings must render the
    // same groups, not merely both render something.
    let by_alias = csv("FIND symbols GROUP BY kind");
    let by_canonical = csv("FIND symbols GROUP BY fql_kind");
    assert!(
        !by_canonical.contains("(empty)"),
        "`GROUP BY fql_kind` itself fabricated a group, so this proves nothing:\n{by_canonical}"
    );
    assert_eq!(
        by_alias, by_canonical,
        "`GROUP BY kind` must render the same groups as `GROUP BY fql_kind`"
    );

    // `node_id` is groupable by the same table and collapsed the same way.
    //
    // A row that genuinely carries no handle — a kind the index does not make
    // addressable — still groups under `(empty)`, and that is an honest key
    // for "this row has no node_id". The defect was every row landing there
    // because the projection resolved the field for none of them.
    // Counted against the ungrouped result rather than asserted to be "more
    // than one": a handle is unique per node, so every row that carries one
    // is its own group, and every row that does not belongs to `(empty)`.
    // Anything looser passes while most rows still fail to resolve.
    let by_node_id = csv("FIND symbols GROUP BY node_id");
    let ungrouped = query(&mut t, "FIND symbols LIMIT 1000");
    let with_handle = ungrouped
        .results
        .iter()
        .filter(|r| r.node_id.is_some())
        .count();
    let without_handle = ungrouped.results.len() - with_handle;

    let resolved_groups = by_node_id
        .lines()
        .filter(|l| l.starts_with("\"n") && l.contains('.'))
        .count();
    assert_eq!(
        resolved_groups, with_handle,
        "every row carrying a handle must be its own group; \
         {with_handle} rows have one and {resolved_groups} groups resolved:\n{by_node_id}"
    );

    let empty_count: usize = by_node_id
        .lines()
        .find_map(|l| l.strip_prefix("\"(empty)\","))
        .map_or(0, |n| n.trim().parse().unwrap_or(0));
    assert_eq!(
        empty_count, without_handle,
        "`(empty)` must hold exactly the rows that carry no handle:\n{by_node_id}"
    );
}

/// The set of rows an outline predicate can act on must not depend on which
/// spelling of the kind field the predicate uses.
///
/// `SHOW outline` lists structural declarations only; a `fql_kind` predicate
/// opts back into every node so the filter has the full tree to act on.
/// `kind` is documented as the same field, so it has to open the same
/// universe — otherwise one spelling searches the whole file and the other
/// searches a subset, and the narrower one answers zero for a node that is
/// plainly there.
///
/// Pinned on a kind the structural tree does **not** contain, derived from the
/// fixture rather than hardcoded. A structural kind cannot detect this: it is
/// present in both universes, so the two spellings agree whether or not the
/// wider one was opened.
#[test]
fn kind_opens_the_same_outline_universe_as_fql_kind() {
    fn values(rendered: &str, key: &str) -> Vec<String> {
        let needle = format!("{key}: \"");
        let mut found: Vec<String> = rendered
            .split(needle.as_str())
            .skip(1)
            .filter_map(|s| s.split('"').next().map(str::to_owned))
            .collect();
        found.sort_unstable();
        found
    }

    let mut t = guarded_workspace(3);
    let mut render = |fql: &str| format!("{:?}", t.try_fql(fql).expect("query"));

    let mut structural = values(&render("SHOW outline OF 'guards.cpp'"), "fql_kind");
    structural.dedup();
    let mut every = values(&render("SHOW outline OF 'guards.cpp' ALL"), "fql_kind");
    every.dedup();

    let outside = every
        .into_iter()
        .find(|k| !structural.contains(k))
        .expect("ALL adds no kind beyond the structural tree here, so this proves nothing");

    let alias = format!("SHOW outline OF 'guards.cpp' WHERE kind = '{outside}'");
    let canonical = format!("SHOW outline OF 'guards.cpp' WHERE fql_kind = '{outside}'");
    let from_alias = values(&render(&alias), "name");
    let from_canonical = values(&render(&canonical), "name");

    assert!(
        !from_canonical.is_empty(),
        "`{canonical}` selected nothing, so this proves nothing"
    );
    assert_eq!(
        from_alias, from_canonical,
        "`kind` must open the same outline universe as `fql_kind`: a kind outside \
         the structural tree ('{outside}') has to be reachable under both spellings"
    );
}
